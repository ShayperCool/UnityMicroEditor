package goproject

import (
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

type Options struct {
	Project       string
	Manifest      string
	NativeCatalog string
	// Scene selects authored YAML while reusing the validated native host/catalog.
	Scene string
	// ExtraAssets includes runtime-addressed prefabs absent from YAML references.
	ExtraAssets []string
	// ScriptTypes can supply types discovered by a new C# compilation.
	ScriptTypes map[string]string
}

// NeedsImportError prevents publishing a scene against stale native content.
type NeedsImportError struct{ Path string }

func (e *NeedsImportError) Error() string {
	return "native content changed; refresh importer catalog: " + e.Path
}

type sourceRoot struct {
	Path   string `json:"path"`
	Source string `json:"source"`
}
type dependency struct {
	Path       string `json:"path"`
	SHA256     string `json:"sha256"`
	Bytes      int64  `json:"bytes"`
	SourcePath string `json:"sourcePath,omitempty"`
}
type catalog struct {
	SchemaVersion int          `json:"schemaVersion"`
	EngineVersion string       `json:"engineVersion"`
	Project       string       `json:"project"`
	SourceRoots   []sourceRoot `json:"sourceRoots"`
	ModelPrefabs  []struct {
		GUID    string     `json:"guid"`
		Path    string     `json:"path"`
		Objects []Document `json:"objects"`
	} `json:"modelPrefabs"`
	Assets                 []map[string]any `json:"assets"`
	PrefabObservations     []map[string]any `json:"prefabObservations"`
	IgnoredPrefabOverrides []map[string]any `json:"ignoredPrefabOverrides"`
	SourceDependencies     []dependency     `json:"sourceDependencies"`
}

// Session owns caches for one project. Native content and managed assembly
// bindings are inherited from a validated host preparation, never rebuilt here.
type Session struct {
	mu                 sync.Mutex
	project            string
	baseline           map[string]any
	catalog            catalog
	files              map[string]fileRecord
	documents          map[string]yamlRecord
	observed           map[string]identity
	stats              CacheStats
	byGUID             map[string]string
	byPath             map[string]string
	models             map[string][]Document
	assets             map[string]map[string]any
	scriptTypes        map[string]string
	dependencies       map[string]dependency
	critical           map[string]string
	prefabObservations map[string]map[string]any
	ignoredOverrides   map[string]bool
	indexDirty         bool
	explicitSources    map[string]string
	nativeOverlays     map[string]NativeReplacement
	extraAssets        []string
}

type Prepared struct {
	Manifest       string     `json:"manifest"`
	Revision       string     `json:"revision"`
	Project        string     `json:"project"`
	Scene          string     `json:"scene"`
	Objects        int        `json:"objectCount"`
	Assets         int        `json:"assetCount"`
	ElapsedSeconds float64    `json:"prepareSeconds"`
	Cache          CacheStats `json:"cache"`
}

func readJSON(path string, value any) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()
	d := json.NewDecoder(io.LimitReader(f, 256<<20))
	d.UseNumber()
	if err := d.Decode(value); err != nil {
		return err
	}
	var extra any
	if err := d.Decode(&extra); !errors.Is(err, io.EOF) {
		return fmt.Errorf("trailing JSON content: %s", path)
	}
	return nil
}

func NewSession(options Options) (*Session, error) {
	project, err := filepath.EvalSymlinks(options.Project)
	if err != nil {
		return nil, err
	}
	project, err = filepath.Abs(project)
	if err != nil {
		return nil, err
	}
	s := &Session{project: project, files: map[string]fileRecord{}, documents: map[string]yamlRecord{}, observed: map[string]identity{}, byGUID: map[string]string{}, byPath: map[string]string{}, models: map[string][]Document{}, assets: map[string]map[string]any{}, scriptTypes: map[string]string{}, critical: map[string]string{}, prefabObservations: map[string]map[string]any{}, ignoredOverrides: map[string]bool{}, indexDirty: true}
	s.explicitSources = map[string]string{}
	s.nativeOverlays = map[string]NativeReplacement{}
	if err := readJSON(options.Manifest, &s.baseline); err != nil {
		return nil, err
	}
	if number(s.baseline["schemaVersion"]) != 1 || s.baseline["supported"] != true {
		return nil, fmt.Errorf("baseline must be a supported schema1 scene manifest")
	}
	if str(s.baseline["projectRoot"]) != project {
		return nil, fmt.Errorf("baseline belongs to another project")
	}
	if !strings.HasPrefix(str(s.baseline["scenePath"]), "Assets/") {
		return nil, fmt.Errorf("scene must be under Assets")
	}
	if options.Scene != "" {
		if err := validateAuthoredPath(options.Scene, ".unity"); err != nil {
			return nil, err
		}
		if _, err := s.source(options.Scene); err != nil {
			return nil, err
		}
		s.baseline["scenePath"] = options.Scene
		s.baseline["sceneName"] = strings.TrimSuffix(filepath.Base(options.Scene), ".unity")
	}
	for _, path := range options.ExtraAssets {
		if err := validateAuthoredPath(path, ".prefab"); err != nil {
			return nil, err
		}
		s.extraAssets = append(s.extraAssets, path)
	}
	if options.NativeCatalog != "" {
		if err := readJSON(options.NativeCatalog, &s.catalog); err != nil {
			return nil, err
		}
		if s.catalog.SchemaVersion != 1 || s.catalog.EngineVersion != str(s.baseline["unityVersion"]) || s.catalog.Project != project {
			return nil, fmt.Errorf("native catalog does not match baseline project and engine")
		}
		for _, r := range s.catalog.SourceRoots {
			if !strings.HasPrefix(r.Path, "Packages/") || filepath.ToSlash(filepath.Clean(r.Path)) != r.Path || !filepath.IsAbs(r.Source) {
				return nil, fmt.Errorf("invalid declared package root %s", r.Path)
			}
		}
		for _, model := range s.catalog.ModelPrefabs {
			s.models[model.GUID] = model.Objects
		}
		for _, d := range s.catalog.SourceDependencies {
			ext := strings.ToLower(filepath.Ext(d.Path))
			if ext != ".unity" && ext != ".prefab" && ext != ".cs" {
				s.critical[d.Path] = d.SHA256
			}
		}
		for _, observation := range s.catalog.PrefabObservations {
			s.prefabObservations[str(observation["guid"])] = observation
		}
		for _, o := range s.catalog.IgnoredPrefabOverrides {
			if o["propertyExists"] == false {
				s.ignoredOverrides[overrideKey(str(o["path"]), str(o["instanceFileId"]), str(o["targetGuid"]), str(o["targetFileId"]), str(o["propertyPath"]))] = true
			}
		}
	}
	for _, a := range array(s.baseline["assets"]) {
		asset := object(a)
		s.assets[assetKey(str(asset["guid"]), str(asset["fileId"]))] = asset
		if str(asset["kind"]) == "script" {
			s.scriptTypes[str(asset["guid"])] = str(asset["typeName"])
		}
		if rawKind(str(asset["kind"])) {
			delete(s.critical, str(asset["path"]))
		}
	}
	for guid, typeName := range options.ScriptTypes {
		s.scriptTypes[guid] = typeName
	}
	for _, v := range array(s.baseline["sourceDependencies"]) {
		d := object(v)
		path := str(d["path"])
		if physical := str(d["sourcePath"]); physical != "" {
			allowed := false
			for _, r := range s.catalog.SourceRoots {
				relative, err := filepath.Rel(r.Source, physical)
				if err == nil && relative != ".." && !strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
					canonical, err := s.checkedPath(physical, r.Source)
					if err != nil {
						return nil, err
					}
					s.explicitSources[path] = canonical
					allowed = true
					break
				}
			}
			if !allowed {
				return nil, fmt.Errorf("sourcePath is outside declared package roots: %s", physical)
			}
		}
		extension := strings.ToLower(filepath.Ext(path))
		// Scene and prefab data are rebuilt below. C# belongs to the compiler.
		if extension != ".unity" && extension != ".prefab" && extension != ".cs" {
			isRaw := false
			for _, a := range s.assets {
				if str(a["path"]) == path && rawKind(str(a["kind"])) {
					isRaw = true
					break
				}
			}
			if !isRaw {
				s.critical[path] = str(d["sha256"])
			}
		}
		if strings.HasSuffix(strings.ToLower(path), ".cs.meta") {
			delete(s.critical, path)
		}
	}
	return s, nil
}

func validateAuthoredPath(path, extension string) error {
	if !strings.HasPrefix(path, "Assets/") || filepath.ToSlash(filepath.Clean(path)) != path || filepath.Ext(path) != extension {
		return fmt.Errorf("authored path must be a canonical Assets/%s source: %s", extension, path)
	}
	return nil
}

func assetKey(guid, id string) string { return guid + "/" + id }
func rawKind(kind string) bool {
	return kind == "prefab" || kind == "script" || kind == "scriptableObject" || kind == "mesh" || kind == "authoredMaterial" || kind == "texture" || kind == "cubemap"
}
func overrideKey(path, instance, guid, id, property string) string {
	return strings.Join([]string{path, instance, guid, id, property}, "\x00")
}
func str(v any) string {
	if v == nil {
		return ""
	}
	switch t := v.(type) {
	case string:
		return t
	case json.Number:
		return string(t)
	default:
		return fmt.Sprint(v)
	}
}
func number(v any) int64 {
	switch t := v.(type) {
	case int:
		return int64(t)
	case int64:
		return t
	case float64:
		return int64(t)
	case json.Number:
		i, _ := t.Int64()
		return i
	}
	return 0
}
func object(v any) map[string]any {
	if t, ok := v.(map[string]any); ok {
		return t
	}
	return map[string]any{}
}
func array(v any) []any {
	if t, ok := v.([]any); ok {
		return t
	}
	return []any{}
}
func fileID(v any) string {
	id := str(object(v)["fileID"])
	if id == "" {
		return "0"
	}
	return id
}

func (s *Session) source(logical string) (string, error) {
	logical = filepath.ToSlash(logical)
	if filepath.IsAbs(logical) || strings.HasPrefix(logical, "../") || strings.Contains(logical, "/../") {
		return "", fmt.Errorf("invalid logical source path %q", logical)
	}
	path := filepath.Join(s.project, filepath.FromSlash(logical))
	if _, err := os.Stat(path); err == nil {
		return s.checkedPath(path, s.project)
	}
	if physical := s.explicitSources[logical]; physical != "" {
		for _, r := range s.catalog.SourceRoots {
			relative, err := filepath.Rel(r.Source, physical)
			if err == nil && relative != ".." && !strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
				return s.checkedPath(physical, r.Source)
			}
		}
	}
	for _, r := range s.catalog.SourceRoots {
		if logical == r.Path || strings.HasPrefix(logical, r.Path+"/") {
			return s.checkedPath(filepath.Join(r.Source, filepath.FromSlash(strings.TrimPrefix(logical, r.Path+"/"))), r.Source)
		}
	}
	return "", fmt.Errorf("source not found: %s", logical)
}

func (s *Session) checkedPath(path, root string) (string, error) {
	canonical, err := filepath.EvalSymlinks(path)
	if err != nil {
		return "", err
	}
	canonical, err = filepath.Abs(canonical)
	if err != nil {
		return "", err
	}
	base, err := filepath.EvalSymlinks(root)
	if err != nil {
		return "", err
	}
	base, err = filepath.Abs(base)
	if err != nil {
		return "", err
	}
	rel, err := filepath.Rel(base, canonical)
	if err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
		return "", fmt.Errorf("source escapes declared root: %s", path)
	}
	library := filepath.Join(s.project, "Library")
	if canonical == library || strings.HasPrefix(canonical, library+string(filepath.Separator)) {
		return "", fmt.Errorf("project Library is not a raw source: %s", path)
	}
	return canonical, nil
}

func (s *Session) indexGUIDs() error {
	if !s.indexDirty {
		return nil
	}
	byGUID, byPath := map[string]string{}, map[string]string{}
	roots := []sourceRoot{{"Assets", filepath.Join(s.project, "Assets")}, {"Packages", filepath.Join(s.project, "Packages")}}
	for _, r := range s.catalog.SourceRoots {
		if _, err := os.Stat(filepath.Join(s.project, filepath.FromSlash(r.Path))); os.IsNotExist(err) {
			roots = append(roots, r)
		}
	}
	for _, r := range roots {
		if _, err := os.Stat(r.Source); os.IsNotExist(err) {
			continue
		}
		err := filepath.WalkDir(r.Source, func(path string, entry os.DirEntry, walkErr error) error {
			if walkErr != nil {
				return walkErr
			}
			if entry.Type()&os.ModeSymlink != 0 {
				return fmt.Errorf("symlinked raw metadata root is unsupported: %s", path)
			}
			if entry.IsDir() || !strings.HasSuffix(path, ".meta") {
				return nil
			}
			source := strings.TrimSuffix(path, ".meta")
			info, err := os.Stat(source)
			if os.IsNotExist(err) || err == nil && info.IsDir() {
				return nil
			}
			if err != nil {
				return err
			}
			content, _, err := s.read(path, true)
			if err != nil {
				return err
			}
			match := guidLine.FindSubmatch(content)
			if match == nil {
				return nil
			}
			guid := strings.ToLower(string(match[1]))
			rel, err := filepath.Rel(r.Source, source)
			if err != nil {
				return err
			}
			logical := r.Path + "/" + filepath.ToSlash(rel)
			if previous := byGUID[guid]; previous != "" && previous != logical {
				return fmt.Errorf("duplicate GUID %s: %s and %s", guid, previous, logical)
			}
			byGUID[guid] = logical
			byPath[logical] = guid
			return nil
		})
		if err != nil {
			return err
		}
	}
	s.byGUID, s.byPath = byGUID, byPath
	s.indexDirty = false
	return nil
}

func (s *Session) depend(logical string) error {
	for _, name := range []string{logical, logical + ".meta"} {
		path, err := s.source(name)
		if err != nil {
			if name != logical {
				if s.byPath[logical] != "" {
					return err
				}
				continue
			}
			return err
		}
		text := map[string]bool{".unity": true, ".prefab": true, ".mat": true, ".asset": true, ".cs": true, ".meta": true, ".txt": true}[strings.ToLower(filepath.Ext(path))]
		_, hash, err := s.read(path, text)
		if err != nil {
			return err
		}
		info, err := os.Stat(path)
		if err != nil {
			return err
		}
		d := dependency{name, hash, info.Size(), ""}
		if !strings.HasPrefix(path, s.project+string(filepath.Separator)) {
			d.SourcePath = path
		}
		s.dependencies[name] = d
		if expected := s.critical[name]; expected != "" && hash != expected {
			return &NeedsImportError{name}
		}
	}
	return nil
}

func (s *Session) Prepare(outputRoot string) (Prepared, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	started := time.Now()
	s.stats = CacheStats{}
	s.observed = map[string]identity{}
	s.dependencies = map[string]dependency{}
	if err := s.validateRetainedBuiltinShaders(); err != nil {
		return Prepared{}, err
	}
	if err := s.indexGUIDs(); err != nil {
		return Prepared{}, err
	}
	// Validate the authored closure once; later attempts read only changed files.
	for _, value := range array(s.baseline["sourceDependencies"]) {
		if err := s.depend(str(object(value)["path"])); err != nil {
			return Prepared{}, err
		}
	}
	scene := str(s.baseline["scenePath"])
	expanded, _, err := s.expand(scene, "", nil)
	if err != nil {
		return Prepared{}, err
	}
	if err := s.validate(expanded); err != nil {
		return Prepared{}, err
	}
	// Published JSON is encoded while the session lock is held. All changed
	// top-level values are replaced below; inherited metadata is read-only.
	// Avoid copying the old scene and runtime prefab graphs just to discard them.
	manifest := make(map[string]any, len(s.baseline))
	for key, value := range s.baseline {
		manifest[key] = value
	}
	objects, err := s.manifestObjects(expanded)
	if err != nil {
		return Prepared{}, err
	}
	manifest["objects"], err = orderSceneRoots(objects)
	if err != nil {
		return Prepared{}, err
	}
	assetList, err := s.prepareAssets(expanded)
	if err != nil {
		return Prepared{}, err
	}
	manifest["assets"] = assetList
	if err := s.publishNativeContent(manifest); err != nil {
		return Prepared{}, err
	}
	deps := make([]dependency, 0, len(s.dependencies))
	for _, d := range s.dependencies {
		deps = append(deps, d)
	}
	sort.Slice(deps, func(i, j int) bool { return deps[i].Path < deps[j].Path })
	manifest["sourceDependencies"] = deps
	depJSON, _ := json.Marshal(deps)
	hash := sha256.Sum256(depJSON)
	manifest["contentHash"] = hex.EncodeToString(hash[:])
	for path, expected := range s.observed {
		actual, err := fileIdentity(path)
		if err != nil || expected != actual {
			return Prepared{}, fmt.Errorf("source changed during preparation, retry: %s", path)
		}
	}
	var id [16]byte
	if _, err := rand.Read(id[:]); err != nil {
		return Prepared{}, err
	}
	revision := hex.EncodeToString(id[:])
	output, err := filepath.Abs(outputRoot)
	if err != nil {
		return Prepared{}, err
	}
	directory := filepath.Join(output, "revisions", revision)
	if err := os.MkdirAll(filepath.Dir(directory), 0755); err != nil {
		return Prepared{}, err
	}
	if err := os.Mkdir(directory, 0755); err != nil {
		return Prepared{}, err
	}
	manifest["revision"] = revision
	manifest["projectRoot"] = s.project
	manifest["requiresEditor"] = false
	manifest["requiresLibrary"] = false
	path := filepath.Join(directory, "scene.json")
	payload, err := json.Marshal(manifest)
	if err != nil {
		os.RemoveAll(directory)
		return Prepared{}, err
	}
	if err := writeNewFile(path, payload); err != nil {
		os.RemoveAll(directory)
		return Prepared{}, err
	}
	prepared := Prepared{path, revision, s.project, scene, len(objects), len(assetList), time.Since(started).Seconds(), s.stats}
	return prepared, nil
}

func writeNewFile(path string, payload []byte) error {
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0644)
	if err != nil {
		return err
	}
	_, writeErr := f.Write(payload)
	syncErr := f.Sync()
	closeErr := f.Close()
	return errors.Join(writeErr, syncErr, closeErr)
}

// ManifestJSON is useful for callers replacing managed assembly bindings before
// launch. Its unknown fields survive the preparation unchanged.
func ManifestJSON(path string) (map[string]any, error) {
	var m map[string]any
	err := readJSON(path, &m)
	return m, err
}

func compactJSON(value any) (string, error) {
	b, err := json.Marshal(value)
	return string(bytes.TrimSpace(b)), err
}
func jsonDecodeString(text string, value any) error {
	d := json.NewDecoder(strings.NewReader(text))
	d.UseNumber()
	return d.Decode(value)
}

// Invalidate accepts watcher events. New/deleted/changed metadata rebuilds the
// GUID index. Existing scene and prefab files are checked by stat on every call.
func (s *Session) Invalidate(paths []string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, path := range paths {
		if strings.HasSuffix(strings.ToLower(path), ".meta") || filepath.Ext(path) == "" {
			s.indexDirty = true
		}
	}
}

// SetManagedBindings updates the baseline after a caller has verified a new C#
// compilation. This package never loads or compiles assemblies.
func (s *Session) SetManagedBindings(assemblies []string, scriptTypes map[string]string, initializers any) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.baseline["assemblies"] = assemblies
	if number(s.baseline["lifecycleBarrierVersion"]) == 1 {
		s.baseline["lifecycleBarrierAssemblies"] = assemblies
	}
	if initializers != nil {
		s.baseline["runtimeInitializers"] = clone(initializers)
	}
	for guid, typeName := range scriptTypes {
		s.scriptTypes[guid] = typeName
	}
}
