// Package authoring edits authored Unity files without reserializing unrelated
// YAML. A Store is bound to one explicit project and serializes its own writers.
package authoring

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"syscall"
	"time"
	"unicode/utf8"
)

const VerifiedUnityVersion = "6000.6.3f1"
const maxSourceBytes = 32 << 20

// Error exposes stable machine-readable failures to CLI and MCP clients.
type Error struct {
	Code    string         `json:"code"`
	Message string         `json:"message"`
	Details map[string]any `json:"details,omitempty"`
}

func (e *Error) Error() string { return e.Code + ": " + e.Message }
func failure(code, message string, details map[string]any) error {
	return &Error{code, message, details}
}

type Store struct {
	root    string
	state   string
	version string
}

func NewStore(project string) (*Store, error) {
	if project == "" {
		return nil, failure("invalid_project", "an explicit Unity project path is required", nil)
	}
	root, err := filepath.Abs(project)
	if err != nil {
		return nil, err
	}
	root, err = filepath.EvalSymlinks(root)
	if err != nil {
		return nil, err
	}
	s := &Store{root: root, state: filepath.Join(root, "Library", "UnityMicroEditor", "authoring")}
	for _, dir := range []string{"Assets", "ProjectSettings"} {
		p, err := s.safePath(dir, true)
		if err != nil {
			return nil, err
		}
		info, err := os.Stat(p)
		if err != nil || !info.IsDir() {
			return nil, failure("invalid_project", "missing Unity project directory "+dir, nil)
		}
	}
	versionPath, err := s.safePath("ProjectSettings/ProjectVersion.txt", false)
	if err != nil {
		return nil, err
	}
	data, err := readLimited(versionPath)
	if err != nil {
		return nil, err
	}
	for _, line := range strings.Split(string(data), "\n") {
		if strings.HasPrefix(line, "m_EditorVersion:") {
			s.version = strings.TrimSpace(strings.TrimPrefix(line, "m_EditorVersion:"))
		}
	}
	if s.version == "" {
		return nil, failure("invalid_project", "ProjectVersion.txt has no m_EditorVersion", nil)
	}
	return s, nil
}
func (s *Store) Root() string { return s.root }

func (s *Store) currentVersion() (string, error) {
	path, err := s.safePath("ProjectSettings/ProjectVersion.txt", false)
	if err != nil {
		return "", err
	}
	data, err := readLimited(path)
	if err != nil {
		return "", err
	}
	for _, line := range strings.Split(string(data), "\n") {
		if strings.HasPrefix(line, "m_EditorVersion:") {
			return strings.TrimSpace(strings.TrimPrefix(line, "m_EditorVersion:")), nil
		}
	}
	return "", failure("invalid_project", "ProjectVersion.txt has no m_EditorVersion", nil)
}

// safePath rejects traversal and symlinks beneath the resolved project root.
// The service assumes the local checkout is trusted; it is not a hostile-user sandbox.
func (s *Store) safePath(relative string, allowDir bool) (string, error) {
	if relative == "" || strings.Contains(relative, "\\") || filepath.IsAbs(relative) || filepath.ToSlash(filepath.Clean(relative)) != relative || relative == "." || strings.HasPrefix(relative, "../") {
		return "", failure("invalid_path", "use a normalized project-relative path", map[string]any{"path": relative})
	}
	current := s.root
	parts := strings.Split(relative, "/")
	for i, part := range parts {
		current = filepath.Join(current, part)
		info, err := os.Lstat(current)
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return "", err
		}
		if info.Mode()&os.ModeSymlink != 0 {
			return "", failure("invalid_path", "symlink paths are unsupported", map[string]any{"path": relative})
		}
		if i < len(parts)-1 && !info.IsDir() {
			return "", failure("invalid_path", "path ancestor is not a directory", nil)
		}
		if i == len(parts)-1 && info.IsDir() && !allowDir {
			return "", failure("invalid_path", "expected a file", nil)
		}
	}
	return current, nil
}
func (s *Store) assetPath(relative string, extensions ...string) (string, error) {
	if !strings.HasPrefix(relative, "Assets/") {
		return "", failure("invalid_path", "authored assets must be beneath Assets", nil)
	}
	resolved, err := s.safePath(relative, false)
	if err != nil {
		return "", err
	}
	ext := strings.ToLower(filepath.Ext(relative))
	valid := false
	for _, allowed := range extensions {
		valid = valid || ext == allowed
	}
	if !valid {
		return "", failure("unsupported_asset", "unsupported authored asset extension", map[string]any{"path": relative})
	}
	return resolved, nil
}
func readLimited(path string) ([]byte, error) {
	info, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	if !info.Mode().IsRegular() || info.Size() > maxSourceBytes {
		return nil, failure("unsupported_asset", "expected a regular text file at most 32 MiB", nil)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	if len(data) > maxSourceBytes || !utf8.Valid(data) {
		return nil, failure("unsupported_asset", "expected a UTF-8 text file at most 32 MiB", nil)
	}
	return data, nil
}

type Source struct {
	Path     string `json:"path"`
	GUID     string `json:"guid,omitempty"`
	Revision string `json:"revision"`
	Content  string `json:"content"`
	Metadata string `json:"metadata"`
}

var guidPattern = regexp.MustCompile(`(?m)^guid:\s*([0-9a-fA-F]{32})[ \t]*\r?$`)

func revision(data, meta []byte) string {
	h := sha256.New()
	h.Write([]byte("unitymicroeditor-authored-v1\x00"))
	digest := sha256.Sum256(data)
	h.Write(digest[:])
	digest = sha256.Sum256(meta)
	h.Write(digest[:])
	return hex.EncodeToString(h.Sum(nil))
}
func (s *Store) readSource(relative string) (Source, error) {
	path, err := s.assetPath(relative, ".unity", ".prefab", ".asset", ".mat", ".cs")
	if err != nil {
		return Source{}, err
	}
	data, err := readLimited(path)
	if errors.Is(err, os.ErrNotExist) {
		return Source{}, failure("not_found", "authored asset does not exist", map[string]any{"path": relative})
	}
	if err != nil {
		return Source{}, err
	}
	metaPath, err := s.safePath(relative+".meta", false)
	if err != nil {
		return Source{}, err
	}
	meta, err := readLimited(metaPath)
	if err != nil {
		return Source{}, failure("invalid_identity", "asset requires an existing valid .meta file", map[string]any{"path": relative, "cause": err.Error()})
	}
	matches := guidPattern.FindAllSubmatch(meta, -1)
	if len(matches) != 1 {
		return Source{}, failure("invalid_identity", "asset metadata requires exactly one GUID", map[string]any{"path": relative})
	}
	return Source{relative, strings.ToLower(string(matches[0][1])), revision(data, meta), string(data), string(meta)}, nil
}
func (s *Store) Read(ctx context.Context, relative string) (source Source, err error) {
	err = s.locked(ctx, func() error { var readErr error; source, readErr = s.readSource(relative); return readErr })
	return
}

// Journal records both old and intended bytes before any source mutation.
// Recovery rolls an interrupted transaction forward when all intended bytes
// are present, or back when files still contain only old/intended bytes.
type journalFile struct {
	Path    string `json:"path"`
	Before  []byte `json:"before,omitempty"`
	After   []byte `json:"after"`
	Existed bool   `json:"existed"`
	Mode    uint32 `json:"mode"`
}
type journal struct {
	OperationID string             `json:"operationId"`
	Fingerprint string             `json:"fingerprint"`
	State       string             `json:"state"`
	Files       []journalFile      `json:"files,omitempty"`
	Result      MutationResult     `json:"result"`
	BatchResult *PublicationResult `json:"batchResult,omitempty"`
}
type MutationResult struct {
	OperationID      string            `json:"operationId"`
	Path             string            `json:"path"`
	GUID             string            `json:"guid"`
	PreviousRevision string            `json:"previousRevision"`
	Revision         string            `json:"revision"`
	Changed          bool              `json:"changed"`
	Replayed         bool              `json:"replayed,omitempty"`
	CreatedIDs       map[string]string `json:"createdIds,omitempty"`
	Changes          []Change          `json:"changes,omitempty"`
}
type Change struct {
	FileID   string `json:"fileId,omitempty"`
	Property string `json:"property,omitempty"`
	Action   string `json:"action"`
	Before   any    `json:"before,omitempty"`
	After    any    `json:"after,omitempty"`
}

func (s *Store) locked(ctx context.Context, action func() error) error {
	stateRelative, _ := filepath.Rel(s.root, s.state)
	if _, err := s.safePath(filepath.ToSlash(stateRelative), true); err != nil {
		return err
	}
	if err := os.MkdirAll(s.state, 0700); err != nil {
		return err
	}
	lockPath, err := s.safePath(filepath.ToSlash(stateRelative)+"/lock", false)
	if err != nil {
		return err
	}
	f, err := os.OpenFile(lockPath, os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return err
	}
	defer f.Close()
	for {
		err = syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
		if err == nil {
			break
		}
		if err != syscall.EWOULDBLOCK && err != syscall.EAGAIN {
			return err
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(10 * time.Millisecond):
		}
	}
	defer syscall.Flock(int(f.Fd()), syscall.LOCK_UN)
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := s.recover(); err != nil {
		return err
	}
	return action()
}
func atomicWrite(path string, data []byte, mode os.FileMode) error {
	f, err := os.CreateTemp(filepath.Dir(path), ".ume-authoring-*")
	if err != nil {
		return err
	}
	temp := f.Name()
	defer os.Remove(temp)
	if err = f.Chmod(mode.Perm()); err == nil {
		_, err = f.Write(data)
	}
	if err == nil {
		err = f.Sync()
	}
	closeErr := f.Close()
	if err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	if err = os.Rename(temp, path); err != nil {
		return err
	}
	dir, err := os.Open(filepath.Dir(path))
	if err != nil {
		return err
	}
	defer dir.Close()
	return dir.Sync()
}
func writeJournal(path string, value journal) error {
	data, err := json.Marshal(value)
	if err != nil {
		return err
	}
	return atomicWrite(path, append(data, '\n'), 0600)
}
func (s *Store) recover() error {
	entries, err := os.ReadDir(s.state)
	if err != nil {
		return err
	}
	for _, entry := range entries {
		if !strings.HasSuffix(entry.Name(), ".json") {
			continue
		}
		journalPath := filepath.Join(s.state, entry.Name())
		if entry.Type()&os.ModeSymlink != 0 {
			return failure("invalid_path", "symlink authoring journal is unsupported", nil)
		}
		data, err := os.ReadFile(journalPath)
		if err != nil {
			return err
		}
		var j journal
		if err := json.Unmarshal(data, &j); err != nil {
			return failure("recovery_required", "invalid authoring journal", map[string]any{"path": journalPath})
		}
		if j.State == "committed" || j.State == "rolled_back" {
			continue
		}
		if j.State != "prepared" {
			return failure("recovery_required", "unknown journal state", map[string]any{"operationId": j.OperationID})
		}
		allAfter := true
		for _, file := range j.Files {
			path, err := s.safePath(file.Path, false)
			if err != nil {
				return err
			}
			current, err := os.ReadFile(path)
			missing := errors.Is(err, os.ErrNotExist)
			if err != nil && !missing {
				return err
			}
			isAfter := !missing && string(current) == string(file.After)
			isBefore := (missing && !file.Existed) || (!missing && file.Existed && string(current) == string(file.Before))
			if !isAfter && !isBefore {
				return failure("outcome_unknown", "external changes prevent transaction recovery", map[string]any{"operationId": j.OperationID, "path": file.Path})
			}
			allAfter = allAfter && isAfter
		}
		if allAfter {
			j.State = "committed"
		} else {
			for _, file := range j.Files {
				path, _ := s.safePath(file.Path, false)
				if file.Existed {
					err = atomicWrite(path, file.Before, os.FileMode(file.Mode))
				} else {
					err = os.Remove(path)
					if errors.Is(err, os.ErrNotExist) {
						err = nil
					}
				}
				if err != nil {
					return err
				}
			}
			j.State = "rolled_back"
		}
		if err := writeJournal(journalPath, j); err != nil {
			return err
		}
	}
	return nil
}

type mutationPlan struct {
	source        Source
	content       []byte
	metadata      []byte
	created       map[string]string
	changes       []Change
	beforePublish func() error
}

func (s *Store) mutate(ctx context.Context, path, expected, operationID string, request any, build func() (mutationPlan, error)) (result MutationResult, err error) {
	if expected == "" || operationID == "" || len(operationID) > 128 || strings.TrimSpace(operationID) != operationID {
		return result, failure("invalid_request", "writes require expectedRevision and operationId of 1 to 128 characters", nil)
	}
	encoded, err := json.Marshal(request)
	if err != nil {
		return result, err
	}
	fingerprintHash := sha256.Sum256(encoded)
	fingerprint := hex.EncodeToString(fingerprintHash[:])
	idHash := sha256.Sum256([]byte(operationID))
	journalPath := filepath.Join(s.state, hex.EncodeToString(idHash[:])+".json")
	err = s.locked(ctx, func() error {
		version, err := s.currentVersion()
		if err != nil {
			return err
		}
		if version != VerifiedUnityVersion {
			return failure("unsupported_unity_version", "source writes require the verified Unity profile", map[string]any{"projectVersion": version, "verifiedVersion": VerifiedUnityVersion})
		}
		if old, readErr := os.ReadFile(journalPath); readErr == nil {
			var j journal
			if err := json.Unmarshal(old, &j); err != nil {
				return err
			}
			if j.Fingerprint != fingerprint {
				return failure("idempotency_conflict", "operationId was already used for different arguments", map[string]any{"operationId": operationID})
			}
			if j.State != "committed" {
				return failure("rolled_back", "interrupted operation was rolled back; use a new operationId", nil)
			}
			result = j.Result
			result.Replayed = true
			return nil
		} else if !errors.Is(readErr, os.ErrNotExist) {
			return readErr
		}
		plan, err := build()
		if err != nil {
			return err
		}
		if plan.source.Revision != expected {
			return failure("revision_conflict", "authored asset or metadata changed", map[string]any{"path": path, "expectedRevision": expected, "actualRevision": plan.source.Revision})
		}
		guid := plan.source.GUID
		if guid == "" {
			if match := guidPattern.FindSubmatch(plan.metadata); match != nil {
				guid = string(match[1])
			}
		}
		result = MutationResult{OperationID: operationID, Path: path, GUID: guid, PreviousRevision: expected, Revision: revision(plan.content, plan.metadata), CreatedIDs: plan.created, Changes: plan.changes}
		result.Changed = plan.source.Content != string(plan.content) || plan.source.Metadata != string(plan.metadata)
		j := journal{OperationID: operationID, Fingerprint: fingerprint, State: "prepared", Result: result}
		for _, item := range []struct {
			path          string
			before, after []byte
		}{{path, []byte(plan.source.Content), plan.content}, {path + ".meta", []byte(plan.source.Metadata), plan.metadata}} {
			absolute, err := s.safePath(item.path, false)
			if err != nil {
				return err
			}
			info, statErr := os.Stat(absolute)
			existed := statErr == nil
			if statErr != nil && !errors.Is(statErr, os.ErrNotExist) {
				return statErr
			}
			mode := os.FileMode(0644)
			if existed {
				mode = info.Mode().Perm()
			}
			current, readErr := os.ReadFile(absolute)
			if readErr != nil && !errors.Is(readErr, os.ErrNotExist) {
				return readErr
			}
			if (plan.source.Revision == "absent" && existed) || (plan.source.Revision != "absent" && (!existed || string(current) != string(item.before))) {
				return failure("revision_conflict", "source changed while preparing the mutation", map[string]any{"path": item.path})
			}
			if !existed || string(item.before) != string(item.after) {
				j.Files = append(j.Files, journalFile{item.path, item.before, item.after, existed, uint32(mode)})
			}
		}
		if plan.beforePublish != nil {
			if err := plan.beforePublish(); err != nil {
				return err
			}
			if plan.source.Revision != "absent" {
				current, err := s.readSource(path)
				if err != nil || current.Revision != expected {
					return failure("revision_conflict", "source or metadata changed during publication guard", map[string]any{"path": path})
				}
			}
			for _, file := range j.Files {
				absolute, err := s.safePath(file.Path, false)
				if err != nil {
					return err
				}
				current, err := os.ReadFile(absolute)
				if file.Existed {
					if err != nil || !bytes.Equal(current, file.Before) {
						return failure("revision_conflict", "source changed during publication guard", map[string]any{"path": file.Path})
					}
				} else if !errors.Is(err, os.ErrNotExist) {
					return failure("revision_conflict", "source appeared during publication guard", map[string]any{"path": file.Path})
				}
			}
		}
		if err := writeJournal(journalPath, j); err != nil {
			return err
		}
		for _, file := range j.Files {
			absolute, err := s.safePath(file.Path, false)
			if err != nil {
				return err
			}
			if err := atomicWrite(absolute, file.After, os.FileMode(file.Mode)); err != nil {
				return failure("outcome_unknown", "write interrupted; retrying recovers the journal", map[string]any{"operationId": operationID, "cause": err.Error()})
			}
		}
		j.State = "committed"
		if err := writeJournal(journalPath, j); err != nil {
			return failure("outcome_unknown", "source written but result journal was not committed", map[string]any{"operationId": operationID, "cause": err.Error()})
		}
		return nil
	})
	return
}

type ProjectInfo struct {
	Project              string   `json:"project"`
	UnityVersion         string   `json:"unityVersion"`
	VerifiedUnityVersion string   `json:"verifiedUnityVersion"`
	VersionMatches       bool     `json:"versionMatches"`
	Scenes               []string `json:"scenes"`
	Prefabs              []string `json:"prefabs"`
	Scripts              []string `json:"scripts"`
	AssemblyDefinitions  []string `json:"assemblyDefinitions"`
	IdentityIssues       []string `json:"identityIssues"`
}

func (s *Store) inspect() (ProjectInfo, error) {
	version, err := s.currentVersion()
	if err != nil {
		return ProjectInfo{}, err
	}
	result := ProjectInfo{Project: s.root, UnityVersion: version, VerifiedUnityVersion: VerifiedUnityVersion, VersionMatches: version == VerifiedUnityVersion, Scenes: []string{}, Prefabs: []string{}, Scripts: []string{}, AssemblyDefinitions: []string{}, IdentityIssues: []string{}}
	guids := map[string]string{}
	err = filepath.WalkDir(filepath.Join(s.root, "Assets"), func(path string, entry os.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		relative, _ := filepath.Rel(s.root, path)
		relative = filepath.ToSlash(relative)
		if entry.Type()&os.ModeSymlink != 0 {
			result.IdentityIssues = append(result.IdentityIssues, relative+": symlink excluded")
			return nil
		}
		if entry.IsDir() {
			return nil
		}
		switch strings.ToLower(filepath.Ext(relative)) {
		case ".unity":
			result.Scenes = append(result.Scenes, relative)
		case ".prefab":
			result.Prefabs = append(result.Prefabs, relative)
		case ".cs":
			result.Scripts = append(result.Scripts, relative)
		case ".asmdef":
			result.AssemblyDefinitions = append(result.AssemblyDefinitions, relative)
		case ".meta":
			data, err := readLimited(path)
			if err != nil {
				return err
			}
			matches := guidPattern.FindAllSubmatch(data, -1)
			if len(matches) != 1 {
				result.IdentityIssues = append(result.IdentityIssues, relative+": metadata requires exactly one GUID")
				break
			}
			guid := strings.ToLower(string(matches[0][1]))
			if previous := guids[guid]; previous != "" {
				result.IdentityIssues = append(result.IdentityIssues, fmt.Sprintf("duplicate GUID %s: %s and %s", guid, previous, relative))
			}
			guids[guid] = relative
		}
		return nil
	})
	for _, collection := range [][]string{result.Scenes, result.Prefabs, result.Scripts, result.AssemblyDefinitions} {
		sort.Strings(collection)
		for _, relative := range collection {
			if _, err := os.Lstat(filepath.Join(s.root, relative) + ".meta"); errors.Is(err, os.ErrNotExist) {
				result.IdentityIssues = append(result.IdentityIssues, relative+": missing metadata")
			}
		}
	}
	sort.Strings(result.IdentityIssues)
	return result, err
}
func (s *Store) Inspect(ctx context.Context) (info ProjectInfo, err error) {
	err = s.locked(ctx, func() error { var inspectErr error; info, inspectErr = s.inspect(); return inspectErr })
	return
}

type Capabilities struct {
	UnityVersion string   `json:"unityVersion"`
	SourceWrites bool     `json:"sourceWrites"`
	Operations   []string `json:"operations"`
	Guarantees   []string `json:"guarantees"`
	Limitations  []string `json:"limitations"`
}

func (s *Store) Capabilities() Capabilities {
	version, err := s.currentVersion()
	if err != nil {
		version = "unavailable"
	}
	return Capabilities{version, version == VerifiedUnityVersion,
		[]string{"paged source/native asset inventory", "authored and recursively resolved scene/prefab hierarchy", "existing serialized scalar/reference/collection property updates", "plain GameObject creation/deletion/reparenting", "explicit RectTransform creation", "supported native and MonoBehaviour component attachment/removal", "source prefab instantiation and scalar/reference override set/revert", "explicit material/ScriptableObject/prefab creation", "revision-checked staged multi-file publication", "script read/create/edit", "preview scene changes without source writes", "undo existing-file and multi-file edits without intervening changes"},
		[]string{"project-bound normalized paths", "preserve existing GUIDs and fileIDs", "preserve YAML bytes outside edited property/document spans", "expected asset and metadata revision", "idempotency survives server restart", "project-wide writer lock", "recoverable file transactions"},
		[]string{"source authoring does not imply compiler or Player success", "direct stripped-object writes and generic prefab-instance property writes remain protected; use explicit override operations", "source prefab create/variant/apply/revert/partial and complete unpack use prefab_edit; native-model prefabs and unsupported structural GameObject changes fail explicitly", "reparent preserves local transform only", "low-level native attachment requires explicit fields or a revision-bound template; component_add reads cached native defaults and companions", "MonoBehaviour attachment requires an existing script GUID and explicit fields", "native components are limited to NativeComponentTypes; cached Canvas can replace an ordinary Transform preserving its identity, inherited Transform replacement requires unpack", "Unity Editor and external writers do not participate in the service lock; multi-file publication does not isolate external readers", "cross-asset references to deleted objects are checked in Assets text YAML and refused; they are not rewritten", "package/SDK component reference validation remains the importer and runtime responsibility", "undo cannot erase newly created files", "inventory revision binds file stat values and metadata; use sourceRevision for exact authored byte guards"}}
}
