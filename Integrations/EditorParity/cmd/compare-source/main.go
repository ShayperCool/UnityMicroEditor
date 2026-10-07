// compare-source checks a complete authored hierarchy against public Editor
// SerializedObject readback captured by SourceAuthoringReference.cs.txt.
package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"math"
	"os"
	"sort"
	"strconv"
	"strings"

	goproject "unitymicroeditor/project"
)

type component struct {
	FileID  string         `json:"fileId"`
	ClassID int            `json:"classId"`
	Type    string         `json:"type"`
	Fields  map[string]any `json:"fields"`
}
type object struct {
	FileID     string      `json:"fileId"`
	Name       string      `json:"name"`
	ParentID   string      `json:"parentId"`
	Active     bool        `json:"active"`
	Components []component `json:"components"`
}
type hierarchy struct {
	Objects    []object              `json:"objects"`
	Total      int                   `json:"total"`
	NextOffset *int                  `json:"nextOffset"`
	Provenance map[string]provenance `json:"provenance"`
}
type provenance struct {
	Identities []goproject.AuthoredIdentity `json:"identities"`
}
type property struct{ Path, Type, Value, ReferenceGUID, ReferenceFileID string }
type editorComponent struct {
	Type               string     `json:"type"`
	SourceFileID       string     `json:"sourceFileId"`
	PrefabSourceGUID   string     `json:"prefabSourceGuid"`
	PrefabSourceFileID string     `json:"prefabSourceFileId"`
	Properties         []property `json:"properties"`
}
type editorObject struct {
	Path               string            `json:"path"`
	SourceFileID       string            `json:"sourceFileId"`
	PrefabSourceGUID   string            `json:"prefabSourceGuid"`
	PrefabSourceFileID string            `json:"prefabSourceFileId"`
	Active             bool              `json:"active"`
	Components         []editorComponent `json:"components"`
}
type editorReport struct {
	SchemaVersion int            `json:"schemaVersion"`
	UnityVersion  string         `json:"unityVersion"`
	SceneDirty    bool           `json:"sceneDirty"`
	Objects       []editorObject `json:"objects"`
	Assets        []editorAsset  `json:"assets"`
}
type editorAsset struct {
	Path       string     `json:"path"`
	GUID       string     `json:"guid"`
	FileID     string     `json:"fileId"`
	Type       string     `json:"type"`
	Properties []property `json:"properties"`
}
type sourceAsset struct {
	Path       string `json:"path"`
	GUID       string `json:"guid"`
	Content    string `json:"content"`
	Truncated  bool   `json:"truncated"`
	NextOffset *int   `json:"nextOffset"`
}
type result struct {
	Success                 bool     `json:"success"`
	Objects                 int      `json:"objects"`
	Components              int      `json:"components"`
	Assets                  int      `json:"assets"`
	PrefabBindings          int      `json:"prefabBindings"`
	Properties              int      `json:"properties"`
	UnrepresentedProperties int      `json:"unrepresentedProperties"`
	Differences             []string `json:"differences"`
}

func readHierarchy(data []byte) (hierarchy, error) {
	var envelope map[string]json.RawMessage
	if err := json.Unmarshal(data, &envelope); err != nil {
		return hierarchy{}, err
	}
	if value := envelope["success"]; string(value) == "false" {
		return hierarchy{}, errors.New("source operation reported failure")
	}
	if string(envelope["isError"]) == "true" || len(envelope["error"]) > 0 && string(envelope["error"]) != "null" {
		return hierarchy{}, errors.New("source operation returned an error")
	}
	if envelope["objects"] != nil {
		var h hierarchy
		decoder := json.NewDecoder(bytes.NewReader(data))
		decoder.UseNumber()
		if err := decoder.Decode(&h); err != nil {
			return h, err
		}
		if h.NextOffset != nil || h.Total != len(h.Objects) {
			return h, errors.New("source hierarchy is incomplete or paged")
		}
		for _, object := range h.Objects {
			for _, component := range object.Components {
				if component.Fields == nil {
					return h, errors.New("source comparison requires includeFields: true")
				}
			}
		}
		return h, nil
	}
	for _, key := range []string{"result", "structuredContent", "data"} {
		if envelope[key] != nil {
			return readHierarchy(envelope[key])
		}
	}
	return hierarchy{}, errors.New("no source hierarchy in reply")
}

func sourceProperty(fields map[string]any, path string) (any, bool) {
	path = strings.ReplaceAll(path, ".Array.data[", "[")
	if strings.HasSuffix(path, ".Array.size") {
		value, ok := sourceProperty(fields, strings.TrimSuffix(path, ".Array.size"))
		array, isArray := value.([]any)
		return len(array), ok && isArray
	}
	var value any = fields
	for _, part := range strings.Split(path, ".") {
		key := part
		if index := strings.IndexByte(part, '['); index >= 0 {
			key = part[:index]
		}
		if key != "" {
			mapping, ok := value.(map[string]any)
			if !ok {
				return nil, false
			}
			value, ok = mapping[key]
			if !ok {
				// Public Material SerializedProperty exposes Unity's YAML
				// single-key dictionary entries as first/second pairs.
				if len(mapping) != 1 || key != "first" && key != "second" || !strings.HasPrefix(path, "m_SavedProperties.m_") {
					return nil, false
				}
				for name, entry := range mapping {
					value = entry
					if key == "first" {
						value = name
					}
				}
			}
		}
		for remainder := strings.TrimPrefix(part, key); remainder != ""; {
			close := strings.IndexByte(remainder, ']')
			if close < 0 || remainder[0] != '[' {
				return nil, false
			}
			index, err := strconv.Atoi(remainder[1:close])
			array, ok := value.([]any)
			if err != nil || !ok || index < 0 || index >= len(array) {
				return nil, false
			}
			value = array[index]
			remainder = remainder[close+1:]
		}
	}
	return value, true
}

func numeric(value any) (float64, bool) {
	n, err := strconv.ParseFloat(fmt.Sprint(value), 64)
	return n, err == nil
}
func equalNumber(a, b float64) bool {
	return math.Abs(a-b) <= 1e-6*math.Max(1, math.Max(math.Abs(a), math.Abs(b)))
}
func propertyEqual(value any, p property) bool {
	switch p.Type {
	case "Integer", "Boolean", "Enum", "ArraySize":
		literal := fmt.Sprint(value)
		if flag, ok := value.(bool); ok {
			literal = "0"
			if flag {
				literal = "1"
			}
		}
		a, err := strconv.ParseInt(literal, 10, 64)
		b, referenceErr := strconv.ParseInt(p.Value, 10, 64)
		return err == nil && referenceErr == nil && a == b
	case "Float":
		a, ok := numeric(value)
		b, err := strconv.ParseFloat(p.Value, 64)
		return ok && err == nil && equalNumber(a, b)
	case "String":
		// Unity writes empty serialized string slots as an empty YAML scalar,
		// which the YAML reader represents as nil. Public stringValue is "".
		if value == nil {
			return p.Value == ""
		}
		return fmt.Sprint(value) == p.Value
	case "Vector2", "Vector3", "Vector4", "Quaternion", "Color":
		mapping, ok := value.(map[string]any)
		if !ok {
			return false
		}
		keys := []string{"x", "y", "z", "w"}
		if p.Type == "Color" {
			keys = []string{"r", "g", "b", "a"}
		}
		parts := strings.Split(p.Value, "|")
		if len(parts) < 2 || len(parts) > len(keys) {
			return false
		}
		for i, part := range parts {
			a, ok := numeric(mapping[keys[i]])
			b, err := strconv.ParseFloat(part, 64)
			if !ok || err != nil || !equalNumber(a, b) {
				return false
			}
		}
		return true
	case "ObjectReference":
		mapping, ok := value.(map[string]any)
		if !ok {
			return false
		}
		id := fmt.Sprint(mapping["fileID"])
		if id == "0" {
			return p.ReferenceFileID == "0"
		}
		if guid, ok := mapping["guid"].(string); ok && guid != "" {
			return guid == p.ReferenceGUID && id == p.ReferenceFileID
		}
		// Expanded prefab composite IDs are runtime read identities. Their
		// scene ownership is checked through complete object/component paths.
		if strings.Contains(id, ":") {
			return true
		}
		return id == p.ReferenceFileID
	default:
		return true
	}
}

func compare(h hierarchy, reference editorReport) result {
	r := result{Differences: []string{}}
	if reference.SchemaVersion != 1 || reference.UnityVersion != "6000.6.3f1" || reference.SceneDirty {
		r.Differences = append(r.Differences, "reference must be a saved scene in the pinned Editor")
		return r
	}
	byID := map[string]object{}
	for _, o := range h.Objects {
		byID[o.FileID] = o
	}
	var pathOf func(string, map[string]bool) (string, error)
	pathOf = func(id string, seen map[string]bool) (string, error) {
		o, ok := byID[id]
		if !ok || seen[id] {
			return "", errors.New("source parent absent or cyclic")
		}
		seen[id] = true
		if o.ParentID == "0" || o.ParentID == "" {
			return o.Name, nil
		}
		parent, err := pathOf(o.ParentID, seen)
		return parent + "/" + o.Name, err
	}
	sources := map[string]object{}
	checkPrefabBinding := func(id, guid, fileID, label string) {
		if !strings.Contains(id, ":") {
			return
		}
		for _, identity := range h.Provenance[id].Identities {
			if guid != "" && identity.GUID == guid && identity.FileID == fileID {
				r.PrefabBindings++
				return
			}
		}
		r.Differences = append(r.Differences, label+" public prefab source GUID/fileID differs")
	}
	for _, o := range h.Objects {
		path, err := pathOf(o.FileID, map[string]bool{})
		if err != nil {
			r.Differences = append(r.Differences, err.Error())
			continue
		}
		if _, duplicate := sources[path]; duplicate {
			r.Differences = append(r.Differences, "ambiguous source path "+path)
		}
		sources[path] = o
	}
	for _, o := range reference.Objects {
		source, ok := sources[o.Path]
		if !ok {
			r.Differences = append(r.Differences, "Editor object absent from source: "+o.Path)
			continue
		}
		delete(sources, o.Path)
		r.Objects++
		checkPrefabBinding(source.FileID, o.PrefabSourceGUID, o.PrefabSourceFileID, o.Path)
		if source.Active != o.Active {
			r.Differences = append(r.Differences, o.Path+" active state differs")
		}
		if !strings.Contains(source.FileID, ":") && source.FileID != o.SourceFileID {
			r.Differences = append(r.Differences, o.Path+" ordinary GameObject fileID differs")
		}
		if len(source.Components) != len(o.Components) {
			r.Differences = append(r.Differences, o.Path+" component count differs")
			continue
		}
		for i, public := range o.Components {
			actual := source.Components[i]
			r.Components++
			checkPrefabBinding(actual.FileID, public.PrefabSourceGUID, public.PrefabSourceFileID, o.Path+"/"+public.Type)
			if actual.ClassID != 114 && public.Type != "UnityEngine."+actual.Type {
				r.Differences = append(r.Differences, o.Path+" component order/type differs: "+public.Type+" vs "+actual.Type)
				continue
			}
			if !strings.Contains(actual.FileID, ":") && actual.FileID != public.SourceFileID {
				r.Differences = append(r.Differences, o.Path+"/"+public.Type+" ordinary component fileID differs")
			}
			for _, p := range public.Properties {
				value, exists := sourceProperty(actual.Fields, p.Path)
				if !exists {
					r.UnrepresentedProperties++
					continue
				}
				if reference, ok := value.(map[string]any); ok && p.Type == "ObjectReference" && strings.Contains(fmt.Sprint(reference["fileID"]), ":") {
					r.UnrepresentedProperties++
					continue
				}
				r.Properties++
				if !propertyEqual(value, p) {
					r.Differences = append(r.Differences, fmt.Sprintf("%s/%s.%s differs: source=%v Editor=%s ref=%s/%s", o.Path, actual.Type, p.Path, value, p.Value, p.ReferenceGUID, p.ReferenceFileID))
				}
			}
		}
	}
	for path := range sources {
		r.Differences = append(r.Differences, "source object absent from Editor: "+path)
	}
	sort.Strings(r.Differences)
	r.Success = len(r.Differences) == 0
	return r
}

func compareAssets(r *result, source []sourceAsset, reference []editorAsset) error {
	assets := map[string]sourceAsset{}
	for _, asset := range source {
		if asset.Truncated || asset.NextOffset != nil || asset.Path == "" {
			return errors.New("asset source comparison requires complete asset_read pages")
		}
		if _, duplicate := assets[asset.Path]; duplicate {
			return errors.New("duplicate source asset path")
		}
		assets[asset.Path] = asset
	}
	for _, public := range reference {
		asset, ok := assets[public.Path]
		if !ok {
			return fmt.Errorf("missing source asset %s", public.Path)
		}
		delete(assets, public.Path)
		documents, err := goproject.ParseUnityYAML([]byte(asset.Content))
		if err != nil {
			return err
		}
		var selected *goproject.Document
		for index := range documents {
			if documents[index].FileID == public.FileID {
				selected = &documents[index]
			}
		}
		if selected == nil || asset.GUID != public.GUID {
			r.Differences = append(r.Differences, public.Path+" asset GUID/fileID differs")
			continue
		}
		r.Assets++
		if selected.ClassID != 114 && public.Type != "UnityEngine."+selected.TypeName {
			r.Differences = append(r.Differences, public.Path+" native asset type differs")
		}
		for _, property := range public.Properties {
			value, exists := sourceProperty(selected.Data, property.Path)
			if !exists {
				r.UnrepresentedProperties++
				continue
			}
			r.Properties++
			if !propertyEqual(value, property) {
				r.Differences = append(r.Differences, fmt.Sprintf("%s.%s differs: source=%v Editor=%s", public.Path, property.Path, value, property.Value))
			}
		}
	}
	if len(assets) != 0 {
		return errors.New("source asset absent from Editor readback")
	}
	sort.Strings(r.Differences)
	r.Success = len(r.Differences) == 0
	return nil
}

func run() error {
	source := flag.String("source", "", "complete scene_hierarchy reply JSON")
	editor := flag.String("editor", "", "public Editor source readback JSON")
	output := flag.String("output", "", "optional report path")
	assetsPath := flag.String("assets", "", "optional JSON array of complete asset_read data objects")
	flag.Parse()
	data, err := os.ReadFile(*source)
	if err != nil {
		return err
	}
	h, err := readHierarchy(data)
	if err != nil {
		return err
	}
	data, err = os.ReadFile(*editor)
	if err != nil {
		return err
	}
	var reference editorReport
	if err = json.Unmarshal(data, &reference); err != nil {
		return err
	}
	result := compare(h, reference)
	if *assetsPath != "" || len(reference.Assets) > 0 {
		var assets []sourceAsset
		if *assetsPath != "" {
			data, err = os.ReadFile(*assetsPath)
			if err != nil {
				return err
			}
			if err = json.Unmarshal(data, &assets); err != nil {
				return err
			}
		}
		if err = compareAssets(&result, assets, reference.Assets); err != nil {
			return err
		}
	}
	encoded, err := json.MarshalIndent(result, "", "  ")
	if err != nil {
		return err
	}
	encoded = append(encoded, '\n')
	if *output != "" {
		if err = os.WriteFile(*output, encoded, 0600); err != nil {
			return err
		}
	}
	os.Stdout.Write(encoded)
	if !result.Success {
		return errors.New("source and public Editor differ")
	}
	return nil
}
func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}
