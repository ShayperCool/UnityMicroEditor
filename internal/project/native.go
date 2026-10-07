package goproject

import (
	"encoding/json"
	"fmt"
	"math"
	"math/big"
	"path/filepath"
	"reflect"
	"sort"
	"strconv"
	"strings"
)

type NativeContentObject struct {
	FileID   string `json:"fileId"`
	TypeName string `json:"typeName"`
}
type NativeContentResource struct {
	Path        string `json:"path"`
	VirtualPath string `json:"virtualPath"`
	SHA256      string `json:"sha256"`
}
type NativeContentDependency struct {
	GUID   string `json:"guid"`
	Global bool   `json:"global"`
}
type NativeMeshCPURetention struct {
	FileID             string `json:"fileId"`
	OriginalIsReadable bool   `json:"originalIsReadable"`
}
type NativeUIBinding struct {
	OwnerFileID        string `json:"ownerFileId"`
	PropertyPath       string `json:"propertyPath"`
	TargetGUID         string `json:"targetGuid"`
	TargetFileID       string `json:"targetFileId"`
	ManagedReferenceID string `json:"managedReferenceId,omitempty"`
}
type NativeContentFile struct {
	GUID             string                    `json:"guid"`
	Path             string                    `json:"path"`
	SHA256           string                    `json:"sha256"`
	Objects          []NativeContentObject     `json:"objects"`
	Resources        []NativeContentResource   `json:"resources"`
	Dependencies     []NativeContentDependency `json:"dependencies"`
	MeshCPURetention []NativeMeshCPURetention  `json:"meshCpuRetention,omitempty"`
	UIBindings       []NativeUIBinding         `json:"uiBindings,omitempty"`
	UIBindingProof   *NativeUIBindingProof     `json:"uiBindingProof,omitempty"`
}
type NativeUIBindingProof struct {
	SchemaVersion  int    `json:"schemaVersion"`
	Mode           string `json:"mode"`
	NativeSHA256   string `json:"nativeSha256"`
	ReaderSHA256   string `json:"readerSha256"`
	DumpSHA256     string `json:"dumpSha256"`
	BindingsSHA256 string `json:"bindingsSha256"`
}
type NativeMeshCPUProjection struct {
	SchemaVersion int    `json:"schemaVersion"`
	Mode          string `json:"mode"`
	SourcePath    string `json:"sourcePath"`
	SourceSHA256  string `json:"sourceSha256"`
	ProofPath     string `json:"proofPath"`
	ProofSHA256   string `json:"proofSha256"`
}
type NativeModelGraphTarget struct {
	FileID string `json:"fileId"`
	GUID   string `json:"guid"`
}
type NativeModelGraphReference struct {
	Property string                   `json:"property"`
	Targets  []NativeModelGraphTarget `json:"targets"`
}
type NativeModelGraphObject struct {
	FileID     string                      `json:"fileId"`
	ClassID    int32                       `json:"classId"`
	References []NativeModelGraphReference `json:"references"`
	Scalars    map[string]any              `json:"scalars"`
}
type NativeReplacement struct {
	GUID                   string                   `json:"guid"`
	SourcePath             string                   `json:"sourcePath"`
	SourceSHA256           string                   `json:"sourceSha256"`
	MetaSHA256             string                   `json:"metaSha256"`
	EnvironmentSHA256      string                   `json:"environmentSha256,omitempty"`
	ConfigSourceSHA256     map[string]string        `json:"configSourceSha256,omitempty"`
	Content                NativeContentFile        `json:"content"`
	ModelGraph             []NativeModelGraphObject `json:"modelGraph,omitempty"`
	ModelGraphSHA256       string                   `json:"modelGraphSha256,omitempty"`
	CompiledShaderSnapshot *CompiledShaderSnapshot  `json:"compiledShaderSnapshot,omitempty"`
	MeshCPUProjection      *NativeMeshCPUProjection `json:"meshCpuProjection,omitempty"`
}

// ReplaceNativeContent accepts an already adapted runtime artifact. It does not
// run importers or modify the baseline/catalog. Source hashes, object identities,
// and FBX authored hierarchy must all match before an overlay becomes visible.
func (s *Session) ReplaceNativeContent(replacement NativeReplacement) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	// This validation has its own read transaction. Preserve preparation counters
	// and observations while retaining useful, freshly verified file hashes.
	observed, stats := s.observed, s.stats
	s.observed, s.stats = map[string]identity{}, CacheStats{}
	defer func() { s.observed, s.stats = observed, stats }()
	if err := s.validateNativeReplacement(replacement); err != nil {
		return err
	}
	newSource := true
	for _, value := range s.assets {
		if str(value["guid"]) == replacement.GUID {
			newSource = false
		}
	}
	for _, value := range s.catalog.Assets {
		if str(value["guid"]) == replacement.GUID {
			newSource = false
		}
	}
	encoded, err := json.Marshal(replacement)
	if err != nil {
		return err
	}
	var owned NativeReplacement
	if err := jsonDecodeString(string(encoded), &owned); err != nil {
		return err
	}
	var modelDocuments []Document
	if strings.EqualFold(filepath.Ext(owned.SourcePath), ".fbx") && len(s.models[owned.GUID]) == 0 {
		modelDocuments, err = s.staticNativeModelDocuments(owned)
		if err != nil {
			return err
		}
	}
	s.nativeOverlays[replacement.GUID] = owned
	if len(modelDocuments) != 0 {
		s.models[owned.GUID] = modelDocuments
	}
	s.critical[replacement.SourcePath] = replacement.SourceSHA256
	s.critical[replacement.SourcePath+".meta"] = replacement.MetaSHA256
	for _, value := range owned.Content.Objects {
		key := assetKey(owned.GUID, value.FileID)
		if existing := s.assets[key]; existing != nil && str(existing["kind"]) == "texture" && value.TypeName == "UnityEngine.Texture2D" {
			// Importer settings can move an existing raw texture outside the
			// verified public decoder profile. Its accepted native descriptor
			// becomes authoritative for subsequent preparation attempts.
			next := clone(existing).(map[string]any)
			next["kind"], next["typeName"], next["classId"] = "imported", value.TypeName, 28
			s.assets[key] = next
		}
	}
	// A source may be outside the baseline closure while still being a genuine
	// project asset. Publish its verified runtime identities alongside the native
	// descriptor so raw references can resolve it on this and later preparations.
	if !newSource {
		return nil
	}
	for _, value := range owned.Content.Objects {
		key := assetKey(owned.GUID, value.FileID)
		if s.assets[key] == nil {
			s.assets[key] = map[string]any{"kind": "imported", "guid": owned.GUID, "path": owned.SourcePath,
				"fileId": value.FileID, "typeName": value.TypeName, "classId": nativeContentClass(value.TypeName),
				"name":       strings.TrimSuffix(filepath.Base(owned.SourcePath), filepath.Ext(owned.SourcePath)),
				"references": []any{}, "dataJson": "{}", "importerJson": "{}"}
		}
	}
	return nil
}

func nativeContentClass(typeName string) int32 {
	switch typeName {
	case "UnityEngine.Texture2D":
		return 28
	case "UnityEngine.AudioClip":
		return 83
	case "UnityEngine.GameObject":
		return 1
	case "UnityEngine.Transform":
		return 4
	case "UnityEngine.MeshRenderer":
		return 23
	case "UnityEngine.MeshFilter":
		return 33
	case "UnityEngine.Mesh":
		return 43
	case "UnityEngine.Shader":
		return 48
	case "UnityEngine.Material":
		return 21
	case "UnityEngine.Font":
		return 128
	case "UnityEngine.UIElements.StyleSheet", "UnityEngine.UIElements.VisualTreeAsset":
		return 114
	}
	return 0
}

func validNativeHex(value string, length int) bool {
	if len(value) != length {
		return false
	}
	for _, c := range value {
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f') {
			return false
		}
	}
	return true
}
func validNativeGUID(guid string) bool {
	return validNativeHex(guid, 32) && guid != strings.Repeat("0", 32) && !builtinGUID(guid)
}
func validNativeID(id string) bool {
	n, err := strconv.ParseInt(id, 10, 64)
	return err == nil && n != 0 && strconv.FormatInt(n, 10) == id
}

func (s *Session) validateNativeReplacement(r NativeReplacement) error {
	if !validNativeGUID(r.GUID) || r.Content.GUID != r.GUID || !validNativeHex(r.SourceSHA256, 64) || !validNativeHex(r.MetaSHA256, 64) {
		return fmt.Errorf("native replacement source identity is invalid")
	}
	path := r.SourcePath
	if !strings.HasPrefix(path, "Assets/") || filepath.ToSlash(filepath.Clean(path)) != path {
		return fmt.Errorf("native replacement source path is invalid: %s", path)
	}
	profile := strings.ToLower(filepath.Ext(path))
	if r.MeshCPUProjection != nil && (profile != ".fbx" || r.MeshCPUProjection.SchemaVersion != 1 || r.MeshCPUProjection.Mode != "native-model-mesh-cpu-v1" ||
		!validNativeHex(r.MeshCPUProjection.SourceSHA256, 64) || !validNativeHex(r.MeshCPUProjection.ProofSHA256, 64)) {
		return fmt.Errorf("native Mesh CPU projection provenance is unsupported")
	}
	newModel := profile == ".fbx" && len(s.models[r.GUID]) == 0
	texture := profile == ".png" || profile == ".jpg" || profile == ".jpeg"
	ui := profile == ".uss" || profile == ".uxml" || profile == ".ttf" || profile == ".otf"
	shader := profile == ".shader"
	for logical, expected := range r.ConfigSourceSHA256 {
		if !texture || logical != "ProjectSettings/EditorSettings.asset" || !validNativeHex(expected, 64) {
			return fmt.Errorf("native configuration source guard is unsupported: %s", logical)
		}
		physical, err := s.source(logical)
		if err != nil {
			return err
		}
		_, actual, err := s.read(physical, true)
		if err != nil || actual != expected {
			return fmt.Errorf("native texture compressor settings changed after artifact validation: %s", logical)
		}
	}
	if profile != ".wav" && profile != ".fbx" && !texture && !ui && !shader {
		return fmt.Errorf("native replacement profile is unsupported: %s", path)
	}
	expected := map[string]string{}
	knownPath := ""
	add := func(guid, id, typeName, source string) error {
		if guid != r.GUID {
			return nil
		}
		if source != path {
			return fmt.Errorf("native GUID %s belongs to %s, not %s", guid, source, path)
		}
		if old := expected[id]; old != "" && old != typeName {
			return fmt.Errorf("catalog has conflicting native object type %s", id)
		}
		if !validNativeID(id) || typeName == "" {
			return fmt.Errorf("catalog native identity is incomplete: %s", id)
		}
		expected[id], knownPath = typeName, source
		return nil
	}
	for _, a := range s.catalog.Assets {
		if err := add(str(a["guid"]), str(a["fileId"]), str(a["typeName"]), str(a["path"])); err != nil {
			return err
		}
	}
	for _, a := range s.assets {
		if str(a["kind"]) == "imported" {
			if err := add(str(a["guid"]), str(a["fileId"]), str(a["typeName"]), str(a["path"])); err != nil {
				return err
			}
		}
	}
	if profile == ".fbx" {
		for _, model := range s.catalog.ModelPrefabs {
			if model.GUID == r.GUID {
				for _, doc := range model.Objects {
					if err := add(model.GUID, doc.FileID, doc.TypeName, model.Path); err != nil {
						return err
					}
				}
			}
		}
	}
	if knownPath == "" || len(expected) == 0 {
		// Index current project metadata rather than trusting caller-provided GUIDs.
		// Refresh once for an asset absent from the previous source index, including
		// duplicate GUID detection. No catalog or baseline is modified.
		if s.byGUID[r.GUID] == "" || s.byPath[path] == "" {
			s.indexDirty = true
		}
		if err := s.indexGUIDs(); err != nil {
			return err
		}
		if s.byGUID[r.GUID] != path || s.byPath[path] != r.GUID {
			return fmt.Errorf("native replacement GUID is not owned by project source %s: %s", path, r.GUID)
		}
		switch {
		case profile == ".wav":
			expected["8300000"] = "UnityEngine.AudioClip"
		case texture:
			expected["2800000"] = "UnityEngine.Texture2D"
		case shader:
			expected["4800000"] = "UnityEngine.Shader"
		case ui:
			if err := validateNativeUIObjects(profile, r); err != nil {
				return err
			}
			for _, value := range r.Content.Objects {
				expected[value.FileID] = value.TypeName
			}
		case profile == ".fbx":
			if _, err := s.staticNativeModelDocuments(r); err != nil {
				return err
			}
			for _, value := range r.Content.Objects {
				expected[value.FileID] = value.TypeName
			}
		default:
			return fmt.Errorf("new native model requires an existing verified hierarchy: %s", path)
		}
	}
	if len(r.Content.Objects) != len(expected) {
		return fmt.Errorf("native object identity set changed for %s: expected %d, got %d", path, len(expected), len(r.Content.Objects))
	}
	found := map[string]bool{}
	for _, o := range r.Content.Objects {
		if !validNativeID(o.FileID) || found[o.FileID] || expected[o.FileID] != o.TypeName {
			return fmt.Errorf("native object %s is missing, duplicated, or changed type in %s", o.FileID, path)
		}
		found[o.FileID] = true
	}
	if profile == ".wav" {
		if len(r.Content.Objects) != 1 || r.Content.Objects[0].TypeName != "UnityEngine.AudioClip" || len(r.ModelGraph) != 0 {
			return fmt.Errorf("WAV replacement must contain one AudioClip")
		}
	} else if texture {
		if len(r.Content.Objects) != 1 || r.Content.Objects[0].FileID != "2800000" || r.Content.Objects[0].TypeName != "UnityEngine.Texture2D" || len(r.ModelGraph) != 0 {
			return fmt.Errorf("texture replacement must contain one Texture2D")
		}
	} else if shader {
		if len(r.Content.Objects) != 1 || r.Content.Objects[0].FileID != "4800000" || r.Content.Objects[0].TypeName != "UnityEngine.Shader" ||
			len(r.ModelGraph) != 0 || r.ModelGraphSHA256 != "" || len(r.Content.Resources) != 0 || len(r.Content.Dependencies) != 0 {
			return fmt.Errorf("standalone Shader replacement must contain only Shader 4800000 without native dependencies")
		}
	} else if ui {
		if err := validateNativeUIObjects(profile, r); err != nil {
			return err
		}
	} else {
		if newModel {
			if !validNativeHex(r.ModelGraphSHA256, 64) || nativeModelGraphSHA256(r.ModelGraph) != r.ModelGraphSHA256 {
				return fmt.Errorf("new native model graph differs from the adapter's verified projection")
			}
			if _, err := s.staticNativeModelDocuments(r); err != nil {
				return err
			}
		} else {
			oldGraph, err := s.nativeModelGraph(r.GUID)
			if err != nil {
				return err
			}
			if err := matchNativeModelGraph(oldGraph, r.ModelGraph); err != nil {
				return fmt.Errorf("FBX authored graph changed in %s: %w", path, err)
			}
		}
	}
	for _, name := range []string{path, path + ".meta"} {
		source, err := s.source(name)
		if err != nil {
			return err
		}
		content, hash, err := s.read(source, strings.HasSuffix(name, ".meta"))
		if err != nil {
			return err
		}
		expectedHash := r.SourceSHA256
		if strings.HasSuffix(name, ".meta") {
			expectedHash = r.MetaSHA256
			match := guidLine.FindSubmatch(content)
			if match == nil || strings.ToLower(string(match[1])) != r.GUID {
				return fmt.Errorf("native metadata GUID changed: %s", name)
			}
		}
		if hash != expectedHash {
			return fmt.Errorf("native source changed after import: %s", name)
		}
	}
	if err := s.verifyNativeArtifacts(r.Content); err != nil {
		return err
	}
	if err := s.validateCompiledShaderSnapshot(r); err != nil {
		return err
	}
	dependencies := map[string]bool{}
	for _, d := range r.Content.Dependencies {
		if dependencies[d.GUID] || d.GUID == r.GUID {
			return fmt.Errorf("duplicate or cyclic native dependency %s", d.GUID)
		}
		dependencies[d.GUID] = true
		if d.Global {
			if !builtinGUID(d.GUID) {
				return fmt.Errorf("native global dependency is not a built-in GUID: %s", d.GUID)
			}
		} else if !validNativeGUID(d.GUID) {
			return fmt.Errorf("native dependency GUID is invalid: %s", d.GUID)
		} else if _, ok := s.nativeOverlays[d.GUID]; !ok {
			return fmt.Errorf("native dependency %s has no validated runtime descriptor", d.GUID)
		}
	}
	files := make(map[string]NativeContentFile, len(s.nativeOverlays)+1)
	for guid, overlay := range s.nativeOverlays {
		files[guid] = overlay.Content
	}
	files[r.GUID] = r.Content
	if err := validateNativeUIBindings(r.Content, files); err != nil {
		return err
	}
	if err := validateNativeDependencyGraph(files); err != nil {
		return err
	}
	for path, stamp := range s.observed {
		actual, err := fileIdentity(path)
		if err != nil || actual != stamp {
			return fmt.Errorf("native file changed during validation: %s", path)
		}
	}
	return nil
}

func validateNativeUIObjects(profile string, r NativeReplacement) error {
	if len(r.ModelGraph) != 0 || r.ModelGraphSHA256 != "" || len(r.Content.Resources) != 0 {
		return fmt.Errorf("native UI/font content must have no model graph or streamed resource")
	}
	counts := map[string]int{}
	for _, value := range r.Content.Objects {
		if !validNativeID(value.FileID) {
			return fmt.Errorf("native UI object ID is invalid")
		}
		counts[value.TypeName]++
	}
	switch profile {
	case ".uss":
		if len(r.Content.Objects) != 1 || counts["UnityEngine.UIElements.StyleSheet"] != 1 {
			return fmt.Errorf("native USS content requires exactly one StyleSheet")
		}
	case ".uxml":
		if len(r.Content.Objects) != 2 || counts["UnityEngine.UIElements.StyleSheet"] != 1 || counts["UnityEngine.UIElements.VisualTreeAsset"] != 1 {
			return fmt.Errorf("native UXML content requires exactly one VisualTreeAsset and its inline StyleSheet")
		}
	case ".ttf", ".otf":
		expected := map[string]string{"12800000": "UnityEngine.Font", "2100000": "UnityEngine.Material", "2800000": "UnityEngine.Texture2D"}
		if len(r.Content.Objects) != len(expected) {
			return fmt.Errorf("native font content requires exactly Font, Material and Texture2D")
		}
		for _, value := range r.Content.Objects {
			if expected[value.FileID] != value.TypeName {
				return fmt.Errorf("native font runtime object identity differs")
			}
		}
	default:
		return fmt.Errorf("unsupported native UI/font profile")
	}
	return nil
}

func validateNativeDependencyGraph(files map[string]NativeContentFile) error {
	state := map[string]uint8{}
	var visit func(string) error
	visit = func(guid string) error {
		if state[guid] == 1 {
			return fmt.Errorf("native dependency cycle contains %s", guid)
		}
		if state[guid] == 2 {
			return nil
		}
		file, exists := files[guid]
		if !exists {
			return fmt.Errorf("native dependency %s has no runtime descriptor", guid)
		}
		state[guid] = 1
		for _, dependency := range file.Dependencies {
			if !dependency.Global {
				if err := visit(dependency.GUID); err != nil {
					return err
				}
			}
		}
		state[guid] = 2
		return nil
	}
	for guid := range files {
		if err := visit(guid); err != nil {
			return err
		}
	}
	return nil
}

func (s *Session) verifyNativeArtifacts(file NativeContentFile) error {
	if err := validateNativeMeshCPURetention(file); err != nil {
		return err
	}
	verify := func(path, hash string) error {
		if !filepath.IsAbs(path) || filepath.Clean(path) != path || !validNativeHex(hash, 64) {
			return fmt.Errorf("native artifact descriptor is invalid: %s", path)
		}
		canonical, err := filepath.EvalSymlinks(path)
		if err != nil {
			return err
		}
		if canonical != path {
			return fmt.Errorf("native artifact is symlinked: %s", path)
		}
		library := filepath.Join(s.project, "Library")
		if path == library || strings.HasPrefix(path, library+string(filepath.Separator)) {
			return fmt.Errorf("native artifact must be independent of project Library")
		}
		_, actual, err := s.read(path, false)
		if err != nil {
			return err
		}
		if actual != hash {
			return fmt.Errorf("native artifact changed after import: %s", path)
		}
		return nil
	}
	if err := verify(file.Path, file.SHA256); err != nil {
		return err
	}
	seen := map[string]bool{}
	for _, r := range file.Resources {
		if r.VirtualPath != "VirtualArtifacts/Primary/"+file.GUID+".resource" || seen[r.VirtualPath] {
			return fmt.Errorf("native resource alias is invalid or duplicated")
		}
		seen[r.VirtualPath] = true
		if err := verify(r.Path, r.SHA256); err != nil {
			return err
		}
	}
	return nil
}

func (s *Session) publishNativeContent(manifest map[string]any) error {
	if len(s.nativeOverlays) == 0 {
		return nil
	}
	files := map[string]NativeContentFile{}
	if value, exists := manifest["nativeContentFiles"]; exists && value != nil {
		encoded, err := json.Marshal(value)
		if err != nil {
			return err
		}
		var existing []NativeContentFile
		if err := jsonDecodeString(string(encoded), &existing); err != nil {
			return err
		}
		for _, file := range existing {
			if _, ok := files[file.GUID]; ok {
				return fmt.Errorf("duplicate baseline native descriptor")
			}
			files[file.GUID] = file
		}
	}
	for guid, r := range s.nativeOverlays {
		if err := s.verifyNativeArtifacts(r.Content); err != nil {
			return err
		}
		// Re-check fresh source expectations even if this GUID is not currently used.
		if err := s.depend(r.SourcePath); err != nil {
			return err
		}
		if err := s.publishCompiledShaderSnapshot(r, manifest); err != nil {
			return err
		}
		files[guid] = r.Content
	}
	keys := make([]string, 0, len(files))
	for key := range files {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	ordered := make([]NativeContentFile, 0, len(keys))
	for _, key := range keys {
		ordered = append(ordered, files[key])
	}
	manifest["nativeContentFiles"] = ordered
	return nil
}

// nativeModelGraph reads the same authored fields consumed by the runtime
// loader. Unsupported native component graphs must go through a new catalog.
func (s *Session) nativeModelGraph(guid string) ([]NativeModelGraphObject, error) {
	docs := s.models[guid]
	if len(docs) == 0 {
		return nil, fmt.Errorf("FBX GUID has no cached model hierarchy: %s", guid)
	}
	graph := make([]NativeModelGraphObject, 0, len(docs))
	for _, d := range docs {
		g := NativeModelGraphObject{FileID: d.FileID, ClassID: int32(d.ClassID), References: []NativeModelGraphReference{}, Scalars: map[string]any{}}
		reference := func(name string, list bool, components bool) error {
			var values []any
			if list {
				values = array(d.Data[name])
				if values == nil {
					return fmt.Errorf("cached model property %s is missing", name)
				}
			} else {
				v, ok := d.Data[name]
				if !ok {
					return fmt.Errorf("cached model property %s is missing", name)
				}
				values = []any{v}
			}
			refs := make([]NativeModelGraphTarget, 0, len(values))
			for _, value := range values {
				if components {
					value = object(value)["component"]
				}
				ref, ok := value.(map[string]any)
				if !ok {
					return fmt.Errorf("cached model reference %s is invalid", name)
				}
				id := fileID(ref)
				external := str(ref["guid"])
				if id == "0" {
					external = ""
				}
				refs = append(refs, NativeModelGraphTarget{id, external})
			}
			g.References = append(g.References, NativeModelGraphReference{name, refs})
			return nil
		}
		scalar := func(name string) error {
			value, ok := d.Data[name]
			if !ok {
				return fmt.Errorf("cached model scalar %s is missing", name)
			}
			g.Scalars[name] = clone(value)
			return nil
		}
		var refs, scalars []string
		switch d.ClassID {
		case 1:
			if err := reference("m_Component", true, true); err != nil {
				return nil, err
			}
			scalars = []string{"m_Name", "m_Layer", "m_TagString", "m_IsActive", "m_StaticEditorFlags"}
		case 4:
			if err := reference("m_GameObject", false, false); err != nil {
				return nil, err
			}
			if err := reference("m_Children", true, false); err != nil {
				return nil, err
			}
			refs = []string{"m_Father"}
			scalars = []string{"m_LocalPosition", "m_LocalRotation", "m_LocalScale"}
		case 33:
			refs = []string{"m_GameObject", "m_Mesh"}
		case 23:
			if err := reference("m_GameObject", false, false); err != nil {
				return nil, err
			}
			if err := reference("m_Materials", true, false); err != nil {
				return nil, err
			}
			refs = []string{"m_StaticBatchRoot", "m_ProbeAnchor", "m_LightProbeVolumeOverride", "m_AdditionalVertexStreams"}
			scalars = []string{"m_Enabled", "m_CastShadows", "m_ReceiveShadows", "m_DynamicOccludee", "m_MotionVectors", "m_LightProbeUsage", "m_ReflectionProbeUsage", "m_RenderingLayerMask", "m_SortingLayerID", "m_SortingOrder"}
		default:
			return nil, fmt.Errorf("cached model class %d has no verified graph profile", d.ClassID)
		}
		for _, name := range refs {
			if err := reference(name, false, false); err != nil {
				return nil, err
			}
		}
		for _, name := range scalars {
			if err := scalar(name); err != nil {
				return nil, err
			}
		}
		graph = append(graph, g)
	}
	return graph, nil
}

func matchNativeModelGraph(expected, actual []NativeModelGraphObject) error {
	if len(expected) != len(actual) || len(actual) == 0 {
		return fmt.Errorf("model object count differs or graph was not verified")
	}
	byID := map[string]NativeModelGraphObject{}
	for _, g := range actual {
		if !validNativeID(g.FileID) {
			return fmt.Errorf("invalid model object ID")
		}
		if _, ok := byID[g.FileID]; ok {
			return fmt.Errorf("duplicate model object ID")
		}
		byID[g.FileID] = g
	}
	for _, old := range expected {
		next, ok := byID[old.FileID]
		if !ok || next.ClassID != old.ClassID {
			return fmt.Errorf("model identity or type differs: %s", old.FileID)
		}
		if !reflect.DeepEqual(old.References, next.References) {
			return fmt.Errorf("model references or order differ: %s", old.FileID)
		}
		if !nativeScalarEqual(old.Scalars, next.Scalars, old.ClassID == 4) {
			return fmt.Errorf("model authored fields differ: %s", old.FileID)
		}
	}
	return nil
}

func nativeScalarEqual(first, second any, float32Pose bool) bool {
	switch a := first.(type) {
	case map[string]any:
		b, ok := second.(map[string]any)
		if !ok || len(a) != len(b) {
			return false
		}
		for key, value := range a {
			other, ok := b[key]
			if !ok || !nativeScalarEqual(value, other, float32Pose) {
				return false
			}
		}
		return true
	case string:
		b, ok := second.(string)
		return ok && a == b
	}
	if first == nil || second == nil {
		return first == second
	}
	if reflect.TypeOf(first).Kind() == reflect.Bool || reflect.TypeOf(second).Kind() == reflect.Bool {
		return first == second
	}
	a, ok := new(big.Rat).SetString(fmt.Sprint(first))
	if !ok {
		return false
	}
	b, ok := new(big.Rat).SetString(fmt.Sprint(second))
	if !ok {
		return false
	}
	if !float32Pose {
		return a.Cmp(b) == 0
	}
	x, _ := a.Float64()
	y, _ := b.Float64()
	px, py := float32(x), float32(y)
	return !math.IsInf(float64(px), 0) && !math.IsInf(float64(py), 0) && px == py
}
