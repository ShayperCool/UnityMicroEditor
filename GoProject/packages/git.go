package packages

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
)

var commitPattern = regexp.MustCompile(`^[0-9a-fA-F]{40}$`)

func isGit(value string) bool {
	return strings.HasPrefix(value, "git+") || strings.HasPrefix(value, "git://") || strings.HasPrefix(value, "file://") || strings.HasPrefix(value, "ssh://") || strings.HasPrefix(value, "git@") || strings.Contains(value, ".git") && (strings.HasPrefix(value, "https://") || strings.HasPrefix(value, "http://"))
}

type gitReference struct{ remote, subdir, revision string }

func parseGit(value string) (gitReference, error) {
	value = strings.TrimPrefix(value, "git+")
	base, revision, _ := strings.Cut(value, "#")
	subdir := ""
	if strings.HasPrefix(base, "git@") {
		if strings.Contains(base, "?") {
			return gitReference{}, errors.New("SCP Git URLs with package path require ssh:// syntax")
		}
		if !strings.Contains(base, ":") {
			return gitReference{}, errors.New("invalid SCP Git URL")
		}
	} else {
		u, err := url.Parse(base)
		if err != nil {
			return gitReference{}, err
		}
		switch u.Scheme {
		case "https", "http", "ssh", "git", "file":
		default:
			return gitReference{}, fmt.Errorf("unsupported Git protocol %q", u.Scheme)
		}
		if u.Scheme != "file" && u.Host == "" {
			return gitReference{}, errors.New("Git URL host absent")
		}
		if u.User != nil && u.Scheme != "ssh" {
			return gitReference{}, errors.New("Git credentials in URL unsupported; use credential configuration")
		}
		for key := range u.Query() {
			if key != "path" {
				return gitReference{}, fmt.Errorf("unsupported Git query %q", key)
			}
		}
		subdir = u.Query().Get("path")
		u.RawQuery = ""
		base = u.String()
	}
	subdir = strings.TrimPrefix(subdir, "/")
	if subdir != "" {
		clean := filepath.ToSlash(filepath.Clean(subdir))
		if clean != subdir || clean == ".." || strings.HasPrefix(clean, "../") || strings.Contains(clean, "\\") || strings.Contains(clean, ":") {
			return gitReference{}, errors.New("unsafe Git package subdirectory")
		}
	}
	if strings.ContainsAny(revision, "\x00\r\n") || strings.HasPrefix(revision, "-") {
		return gitReference{}, errors.New("unsafe Git revision")
	}
	return gitReference{base, subdir, revision}, nil
}

type gitReceipt struct{ Requested, Revision, Subdirectory, Path, ContentHash, Name, Version string }

func (r *resolver) gitPackage(name, request string) (Package, error) {
	ref, err := parseGit(request)
	if err != nil {
		return Package{}, err
	}
	locked := r.lock.Dependencies[name]
	commit := ""
	if !r.options.RefreshGit && locked.Source == "git" && locked.Version == request {
		commit = locked.Hash
		if !commitPattern.MatchString(commit) {
			return Package{}, errors.New("locked Git revision must be a full commit hash")
		}
	}
	if commit == "" && commitPattern.MatchString(ref.revision) {
		commit = strings.ToLower(ref.revision)
	}
	key := hashBytes([]byte(name + "\x00" + request + "\x00" + commit))
	receiptPath := filepath.Join(r.cache, "git", key+".json")
	var receipt gitReceipt
	unlockedMovingRef := commit == "" && r.manifest.EnableLockFile != nil && !*r.manifest.EnableLockFile
	if _, err = readJSON(receiptPath, &receipt); err == nil && !r.options.RefreshGit && !unlockedMovingRef {
		if receipt.Name != name || receipt.Requested != request || !commitPattern.MatchString(receipt.Revision) || commit != "" && receipt.Revision != commit || !inside(filepath.Join(r.cache, "content"), receipt.Path) {
			return Package{}, errors.New("Git cache receipt identity mismatch")
		}
		p, e := r.fromPath(name, receipt.Path, "git", request, "verified_git_commit")
		if e != nil {
			return p, e
		}
		if p.ContentHash != receipt.ContentHash || p.Version != receipt.Version {
			return p, errors.New("verified Git cache content changed")
		}
		p.Revision = receipt.Revision
		p.Subdirectory = receipt.Subdirectory
		return r.gitMaterializedCopy(p)
	}
	if r.options.Offline {
		return Package{}, fmt.Errorf("offline Git package lacks verified commit/content receipt: %s", name)
	}
	if err = r.checkCacheBudget(2*r.options.MaxDownloadBytes + r.options.MaxExpandedBytes); err != nil {
		return Package{}, err
	}
	repo, err := os.MkdirTemp(r.cache, ".git-fetch-")
	if err != nil {
		return Package{}, err
	}
	defer os.RemoveAll(repo)
	if _, err = r.gitCommand(repo, "init", "--bare"); err != nil {
		return Package{}, err
	}
	if _, err = r.gitCommand(repo, "remote", "add", "origin", ref.remote); err != nil {
		return Package{}, err
	}
	revision := commit
	if revision == "" {
		revision = ref.revision
	}
	if revision == "" {
		revision = "HEAD"
	}
	if _, err = r.gitCommand(repo, "-c", "protocol.file.allow=always", "fetch", "--depth=1", "--filter=blob:none", "--no-tags", "--", "origin", revision); err != nil {
		return Package{}, err
	}
	resolved, err := r.gitCommand(repo, "rev-parse", "--verify", "FETCH_HEAD^{commit}")
	if err != nil {
		return Package{}, err
	}
	resolved = strings.TrimSpace(resolved)
	if !commitPattern.MatchString(resolved) || commit != "" && resolved != commit {
		return Package{}, errors.New("Git resolved commit differs from lock")
	}
	object := resolved
	if ref.subdir != "" {
		object += ":" + ref.subdir
	}
	ctx, cancel := context.WithTimeout(r.ctx, 2*time.Minute)
	defer cancel()
	cmd := exec.CommandContext(ctx, "git", "-C", repo, "archive", "--format=tar", object)
	cmd.Env = gitEnvironment()
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return Package{}, err
	}
	stderr := &boundedBuffer{limit: 65536}
	cmd.Stderr = stderr
	if err = cmd.Start(); err != nil {
		return Package{}, err
	}
	p, extractErr := r.extract(name, io.LimitReader(stdout, r.options.MaxExpandedBytes+int64(r.options.MaxFiles)*2048+1), "git", request, "verified_git_commit", false)
	if extractErr != nil {
		cancel()
	}
	waitErr := cmd.Wait()
	if extractErr != nil {
		return p, extractErr
	}
	if waitErr != nil {
		return p, fmt.Errorf("Git package archive failed: %s", stderr.String())
	}
	if err = r.checkCacheBudget(0); err != nil {
		return p, err
	}
	if err = checkGitMaterialized(p.Path); err != nil {
		return p, err
	}
	p.Revision = resolved
	p.Subdirectory = ref.subdir
	receipt = gitReceipt{request, resolved, ref.subdir, p.Path, p.ContentHash, p.Name, p.Version}
	if err = atomicJSON(receiptPath, receipt); err != nil {
		return p, err
	}
	return r.gitMaterializedCopy(p)
}

// A project PackageCache copy is reusable only after comparison with a freshly
// verified exact-commit archive or its unchanged receipt. Its folder suffix is
// deliberately not treated as a Git revision or as evidence of content.
func (r *resolver) gitMaterializedCopy(commit Package) (Package, error) {
	var paths []string
	for _, path := range r.caches[commit.Name][commit.Version] {
		if inside(filepath.Join(r.project, "Library", "PackageCache"), path) {
			paths = append(paths, path)
		}
	}
	if len(paths) == 0 {
		return commit, nil
	}
	verified, err := r.comparableHash(commit.Path)
	if err != nil {
		return Package{}, err
	}
	sort.Strings(paths)
	var result Package
	for _, path := range paths {
		candidate, err := r.fromPath(commit.Name, path, "git", commit.Requested, "verified_git_cache_copy")
		if err != nil {
			return Package{}, err
		}
		h, err := r.comparableHash(candidate.Path)
		if err != nil {
			return Package{}, err
		}
		if h != verified {
			// The same package version may legitimately exist at another Git
			// commit. Use our verified commit rather than a nonmatching copy.
			continue
		}
		if result.Path != "" && result.ContentHash != candidate.ContentHash {
			return Package{}, errors.New("ambiguous materialized Git package contents")
		}
		result = candidate
	}
	if result.Path == "" {
		return commit, nil
	}
	result.Revision, result.Subdirectory = commit.Revision, commit.Subdirectory
	result.CommitPath, result.CommitContentHash = commit.Path, commit.ContentHash
	r.inputs[filepath.Join(commit.Path, "package.json")] = commit.ManifestHash
	return result, nil
}
func checkGitMaterialized(root string) error {
	return filepath.WalkDir(root, func(path string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if d.IsDir() {
			return nil
		}
		if d.Name() == ".gitmodules" {
			return errors.New("Git submodule packages require explicit materialization; unsupported")
		}
		f, err := os.Open(path)
		if err != nil {
			return err
		}
		var prefix [128]byte
		n, readErr := f.Read(prefix[:])
		f.Close()
		if readErr != nil && readErr != io.EOF {
			return readErr
		}
		if strings.HasPrefix(string(prefix[:n]), "version https://git-lfs.github.com/spec/v1") {
			return errors.New("Git LFS pointer is not materialized; unsupported")
		}
		return nil
	})
}
func gitEnvironment() []string {
	return append(os.Environ(), "GIT_TERMINAL_PROMPT=0", "GIT_CONFIG_NOSYSTEM=1", "GIT_LFS_SKIP_SMUDGE=1")
}

type boundedBuffer struct {
	bytes.Buffer
	limit int
}

func (b *boundedBuffer) Write(p []byte) (int, error) {
	n := len(p)
	if b.Len() < b.limit {
		keep := b.limit - b.Len()
		if keep > len(p) {
			keep = len(p)
		}
		b.Buffer.Write(p[:keep])
	}
	return n, nil
}
func (r *resolver) gitCommand(repo string, args ...string) (string, error) {
	ctx, cancel := context.WithTimeout(r.ctx, 2*time.Minute)
	defer cancel()
	arguments := append([]string{"-C", repo}, args...)
	cmd := exec.CommandContext(ctx, "git", arguments...)
	cmd.Env = gitEnvironment()
	output := &boundedBuffer{limit: 65536}
	cmd.Stdout = output
	cmd.Stderr = output
	if err := cmd.Start(); err != nil {
		return "", err
	}
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case err := <-done:
			if err != nil {
				return "", fmt.Errorf("Git operation failed: %s", output.String())
			}
			return output.String(), nil
		case <-ticker.C:
			var size int64
			_ = filepath.WalkDir(repo, func(path string, d os.DirEntry, err error) error {
				if err != nil {
					return err
				}
				if !d.IsDir() {
					info, e := d.Info()
					if e != nil {
						return e
					}
					size += info.Size()
					if size > r.options.MaxDownloadBytes {
						return errors.New("Git fetch exceeds download budget")
					}
				}
				return nil
			})
			if size > r.options.MaxDownloadBytes {
				cancel()
				<-done
				return "", errors.New("Git repository fetch exceeds download bound")
			}
		case <-ctx.Done():
			cancel()
			<-done
			return "", ctx.Err()
		}
	}
}
