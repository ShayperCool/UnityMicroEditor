package goproject

import (
	"encoding/json"
	"fmt"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
)

type expandedObject struct {
	Document
	SourcePath, GUID       string
	Targets                map[string]bool
	Removed, MissingScript bool
}

type instanceExpansion struct {
	instance   Document
	objects    []*expandedObject
	lookup     map[string]*expandedObject
	sourceGUID string
}

func transform(class int) bool         { return class == 4 || class == 224 }
func targetKey(guid, id string) string { return guid + "/" + id }
func builtinGUID(guid string) bool {
	return guid == "0000000000000000e000000000000000" || guid == "0000000000000000f000000000000000"
}

func (s *Session) documentSource(logical string) ([]Document, error) {
	if err := s.depend(logical); err != nil {
		return nil, err
	}
	guid := s.byPath[logical]
	if model, ok := s.models[guid]; ok {
		docs := cloneDocuments(model)
		local := map[string]bool{}
		native := map[string]bool{}
		for _, d := range docs {
			local[d.FileID] = true
		}
		for _, a := range s.catalog.Assets {
			if str(a["guid"]) == guid {
				native[str(a["fileId"])] = true
			}
		}
		for _, a := range s.assets {
			if str(a["kind"]) == "imported" && str(a["guid"]) == guid {
				native[str(a["fileId"])] = true
			}
		}
		if replacement, exists := s.nativeOverlays[guid]; exists {
			for _, value := range replacement.Content.Objects {
				native[value.FileID] = true
			}
		}
		for i := range docs {
			visitReferences(docs[i].Data, "", func(_ string, r map[string]any) {
				id := fileID(r)
				if str(r["guid"]) == "" && id != "0" && !local[id] && native[id] {
					r["guid"] = guid
					r["type"] = int64(3)
				}
			})
		}
		return docs, nil
	}
	if strings.ToLower(filepath.Ext(logical)) != ".unity" && strings.ToLower(filepath.Ext(logical)) != ".prefab" {
		if strings.ToLower(filepath.Ext(logical)) == ".fbx" {
			return nil, &NeedsImportError{Path: logical}
		}
		return nil, fmt.Errorf("native catalog lacks model graph: %s", logical)
	}
	path, err := s.source(logical)
	if err != nil {
		return nil, err
	}
	return s.parse(path)
}

func (s *Session) expand(logical, prefix string, stack []string) ([]*expandedObject, map[string]string, error) {
	if len(stack) > 64 {
		return nil, nil, fmt.Errorf("prefab depth exceeds 64: %s", logical)
	}
	for _, path := range stack {
		if path == logical {
			return nil, nil, fmt.Errorf("recursive prefab dependency: %s", logical)
		}
	}
	documents, err := s.documentSource(logical)
	if err != nil {
		return nil, nil, err
	}
	guid := s.byPath[logical]
	if guid == "" {
		return nil, nil, fmt.Errorf("source has no GUID metadata: %s", logical)
	}
	own, children := []*expandedObject{}, []*expandedObject{}
	mapping := map[string]string{}
	for _, d := range documents {
		if d.ClassID == 1001 || d.Stripped {
			continue
		}
		id := d.FileID
		d.FileID = prefix + id
		o := &expandedObject{Document: d, SourcePath: logical, GUID: guid, Targets: map[string]bool{targetKey(guid, id): true}}
		mapping[id] = d.FileID
		own = append(own, o)
	}
	instances := []instanceExpansion{}
	for _, instance := range documents {
		if instance.ClassID != 1001 {
			continue
		}
		sourceGUID := str(object(instance.Data["m_SourcePrefab"])["guid"])
		source := s.byGUID[sourceGUID]
		if source == "" {
			return nil, nil, fmt.Errorf("unresolved prefab source %s in %s", sourceGUID, logical)
		}
		objects, _, err := s.expand(source, prefix+instance.FileID+":", append(stack, logical))
		if err != nil {
			return nil, nil, err
		}
		lookup := map[string]*expandedObject{}
		for _, o := range objects {
			for t := range o.Targets {
				lookup[t] = o
			}
		}
		instances = append(instances, instanceExpansion{instance, objects, lookup, sourceGUID})
		children = append(children, objects...)
	}
	for _, d := range documents {
		if !d.Stripped {
			continue
		}
		instanceID := fileID(d.Data["m_PrefabInstance"])
		ref := object(d.Data["m_CorrespondingSourceObject"])
		var target *expandedObject
		for _, i := range instances {
			if i.instance.FileID == instanceID {
				target = i.lookup[targetKey(str(ref["guid"]), fileID(ref))]
				break
			}
		}
		if target == nil {
			return nil, nil, fmt.Errorf("cannot resolve stripped object %s in %s", d.FileID, logical)
		}
		mapping[d.FileID] = target.FileID
		target.Targets[targetKey(guid, d.FileID)] = true
	}
	if strings.ToLower(filepath.Ext(logical)) == ".prefab" {
		for _, o := range children {
			var identity int64
			for _, part := range strings.Split(strings.TrimPrefix(o.FileID, prefix), ":") {
				n, err := strconv.ParseInt(part, 10, 64)
				if err != nil {
					return nil, nil, err
				}
				identity ^= n
			}
			id := strconv.FormatInt(identity&0x7fffffffffffffff, 10)
			if previous := mapping[id]; previous != "" && previous != o.FileID {
				return nil, nil, fmt.Errorf("inherited prefab identity collision %s in %s", id, logical)
			}
			mapping[id] = o.FileID
			o.Targets[targetKey(guid, id)] = true
		}
	}
	rewrite := func(value any) any { return rewriteReferences(value, guid, mapping) }
	for _, o := range own {
		o.Data = rewrite(o.Data).(map[string]any)
	}
	instanceRoots := map[string]string{}
	for _, instance := range instances {
		mod := object(instance.instance.Data["m_Modification"])
		for _, v := range array(mod["m_Modifications"]) {
			override := object(v)
			ref := object(override["target"])
			targetGUID := str(ref["guid"])
			if targetGUID == "" {
				targetGUID = instance.sourceGUID
			}
			target := instance.lookup[targetKey(targetGUID, fileID(ref))]
			if target == nil {
				observation := s.prefabObservations[targetGUID]
				missing := observation["complete"] == true
				if native, known := s.nativeOverlays[targetGUID]; known && len(native.ModelGraph) != 0 {
					// The verified native import is a complete current model. Unity
					// ignores saved overrides for objects removed by a reimport.
					// Keep rejecting existing native objects absent from expansion.
					missing = true
					for _, item := range native.Content.Objects {
						if item.FileID == fileID(ref) {
							missing = false
							break
						}
					}
				} else {
					for _, id := range array(observation["fileIds"]) {
						if str(id) == fileID(ref) {
							missing = false
							break
						}
					}
				}
				if missing {
					continue
				}
				return nil, nil, fmt.Errorf("unresolved prefab override %s/%s in %s", targetGUID, fileID(ref), logical)
			}
			property := str(override["propertyPath"])
			r := object(rewrite(override["objectReference"]))
			if err := applyOverride(target.Data, property, override["value"], r); err != nil {
				if s.ignoredOverrides[overrideKey(logical, instance.instance.FileID, targetGUID, fileID(ref), property)] {
					continue
				}
				return nil, nil, fmt.Errorf("%s override %s: %w", logical, property, err)
			}
		}
		all := append(append([]*expandedObject{}, own...), children...)
		if err := applyStructuralOverrides(instance, all, rewrite); err != nil {
			return nil, nil, fmt.Errorf("%s: %w", logical, err)
		}
		roots := []*expandedObject{}
		for _, o := range instance.objects {
			if !o.Removed && transform(o.ClassID) && fileID(o.Data["m_Father"]) == "0" {
				roots = append(roots, o)
			}
		}
		if len(roots) > 1 {
			return nil, nil, fmt.Errorf("prefab %s has multiple roots", instance.sourceGUID)
		}
		if len(roots) == 1 {
			parent := mod["m_TransformParent"]
			if parent == nil {
				parent = map[string]any{"fileID": "0"}
			}
			roots[0].Data["m_Father"] = rewrite(parent)
			instanceRoots[instance.instance.FileID] = roots[0].FileID
		}
	}
	result := []*expandedObject{}
	for _, o := range append(own, children...) {
		if !o.Removed {
			result = append(result, o)
		}
	}
	for _, o := range own {
		if o.ClassID == 1660057539 {
			for _, v := range array(o.Data["m_Roots"]) {
				r := object(v)
				if root := instanceRoots[fileID(r)]; root != "" {
					r["fileID"] = root
				}
			}
		}
	}
	if strings.ToLower(filepath.Ext(logical)) == ".prefab" {
		roots := []*expandedObject{}
		for _, o := range result {
			if transform(o.ClassID) && fileID(o.Data["m_Father"]) == "0" {
				roots = append(roots, o)
			}
		}
		if len(roots) == 1 {
			owner := fileID(roots[0].Data["m_GameObject"])
			for _, o := range result {
				if o.ClassID == 1 && o.FileID == owner {
					o.Data["m_Name"] = strings.TrimSuffix(filepath.Base(logical), filepath.Ext(logical))
				}
			}
		}
	}
	// Missing MonoScripts may be ignored only with matching Editor observations.
	observation := s.prefabObservations[guid]
	for _, v := range array(observation["missingComponents"]) {
		missing := object(v)
		if missing["observedMissing"] != true {
			continue
		}
		owner := mapping[str(missing["gameObjectFileId"])]
		slot := int(number(missing["componentIndex"]))
		for _, o := range own {
			if o.FileID != owner {
				continue
			}
			components := array(o.Data["m_Component"])
			if slot < 0 || slot >= len(components) {
				continue
			}
			id := fileID(object(components[slot])["component"])
			for _, candidate := range own {
				if candidate.FileID == id && candidate.ClassID == 114 {
					script := str(object(candidate.Data["m_Script"])["guid"])
					if s.byGUID[script] == "" && !builtinGUID(script) {
						candidate.MissingScript = true
					}
				}
			}
		}
	}
	return result, mapping, nil
}

var editorKeys = map[string]bool{"m_CorrespondingSourceObject": true, "m_PrefabInstance": true, "m_PrefabAsset": true}

func rewriteReferences(value any, guid string, mapping map[string]string) any {
	switch v := value.(type) {
	case map[string]any:
		if _, ref := v["fileID"]; ref {
			r := clone(v).(map[string]any)
			original := fileID(r)
			if original != "0" && (str(r["guid"]) == "" || str(r["guid"]) == guid) {
				if id := mapping[original]; id != "" {
					r["fileID"] = id
					delete(r, "guid")
					delete(r, "type")
				}
			}
			return r
		}
		r := map[string]any{}
		for k, item := range v {
			if !editorKeys[k] {
				r[k] = rewriteReferences(item, guid, mapping)
			}
		}
		return r
	case []any:
		r := make([]any, len(v))
		for i, item := range v {
			r[i] = rewriteReferences(item, guid, mapping)
		}
		return r
	default:
		return value
	}
}

var arrayPath = regexp.MustCompile(`\.Array\.data\[(\d+)\]`)
var propertyToken = regexp.MustCompile(`([^\.\[\]]+)|\[(\d+)\]`)
var integerValue = regexp.MustCompile(`^-?\d+$`)

func applyOverride(data map[string]any, path string, raw any, reference map[string]any) error {
	resize := strings.HasSuffix(path, ".Array.size")
	path = strings.TrimSuffix(path, ".Array.size")
	path = arrayPath.ReplaceAllString(path, "[$1]")
	tokens := propertyToken.FindAllStringSubmatch(path, -1)
	if len(tokens) == 0 {
		return fmt.Errorf("empty property path")
	}
	hasIndex := false
	for _, t := range tokens {
		if t[2] != "" {
			hasIndex = true
		}
	}
	var current any = data
	for _, t := range tokens[:len(tokens)-1] {
		if t[2] != "" {
			items, ok := current.([]any)
			i, _ := strconv.Atoi(t[2])
			if !ok || i >= len(items) {
				return fmt.Errorf("array index %s does not exist", t[2])
			}
			current = items[i]
		} else {
			m, ok := current.(map[string]any)
			if !ok {
				return fmt.Errorf("%s is not a mapping", t[1])
			}
			v, exists := m[t[1]]
			if !exists {
				if !hasIndex {
					return fmt.Errorf("field %s does not exist", t[1])
				}
				v = map[string]any{}
				m[t[1]] = v
			}
			current = v
		}
	}
	last := tokens[len(tokens)-1]
	var old any
	var set func(any)
	if last[2] != "" {
		items, ok := current.([]any)
		i, _ := strconv.Atoi(last[2])
		if !ok || i >= len(items) {
			return fmt.Errorf("array index %s does not exist", last[2])
		}
		old = items[i]
		set = func(v any) { items[i] = v }
	} else {
		m, ok := current.(map[string]any)
		if !ok {
			return fmt.Errorf("property parent is not a mapping")
		}
		var exists bool
		old, exists = m[last[1]]
		if !exists && !hasIndex && !(last[1] == "m_RootOrder" && m["m_LocalPosition"] != nil) {
			return fmt.Errorf("field %s does not exist", last[1])
		}
		set = func(v any) { m[last[1]] = v }
	}
	if resize {
		items, ok := old.([]any)
		if !ok {
			return fmt.Errorf("Array.size requires an array")
		}
		size, err := strconv.Atoi(str(raw))
		if err != nil || size < 0 || size > 100000 {
			return fmt.Errorf("invalid array size")
		}
		result := make([]any, size)
		for i := 0; i < size; i++ {
			if i < len(items) {
				result[i] = items[i]
			} else if len(items) > 0 {
				result[i] = clone(items[len(items)-1])
			} else {
				result[i] = map[string]any{}
			}
		}
		set(result)
		return nil
	}
	var value any
	switch v := old.(type) {
	case map[string]any:
		if _, ok := v["fileID"]; !ok {
			return fmt.Errorf("only scalar or reference overrides are supported")
		}
		value = reference
	case bool:
		value = str(raw) == "1" || strings.EqualFold(str(raw), "true")
	case int, int64, json.Number:
		if strings.ContainsAny(str(raw), ".eE") {
			n, err := strconv.ParseFloat(str(raw), 64)
			if err != nil {
				return err
			}
			value = n
		} else {
			n, err := strconv.ParseInt(str(raw), 10, 64)
			if err != nil {
				return err
			}
			value = n
		}
	case uint64:
		n, err := strconv.ParseUint(str(raw), 10, 64)
		if err != nil {
			return err
		}
		value = n
	case float64:
		n, err := strconv.ParseFloat(str(raw), 64)
		if err != nil {
			return err
		}
		value = n
	case string:
		value = str(raw)
	case nil:
		if str(reference["guid"]) != "" || fileID(reference) != "0" {
			value = reference
		} else if raw == nil {
			value = ""
		} else if integerValue.MatchString(str(raw)) {
			n, err := strconv.ParseInt(str(raw), 10, 64)
			if err != nil {
				return err
			}
			value = n
		} else if n, err := strconv.ParseFloat(str(raw), 64); err == nil {
			value = n
		} else {
			value = str(raw)
		}
	default:
		return fmt.Errorf("only scalar or reference overrides are supported")
	}
	set(value)
	return nil
}

func applyStructuralOverrides(instance instanceExpansion, all []*expandedObject, rewrite func(any) any) error {
	mod := object(instance.instance.Data["m_Modification"])
	removed := map[string]bool{}
	byID := map[string]*expandedObject{}
	for _, o := range all {
		byID[o.FileID] = o
	}
	target := func(r any) *expandedObject {
		ref := object(r)
		guid := str(ref["guid"])
		if guid == "" {
			guid = instance.sourceGUID
		}
		return instance.lookup[targetKey(guid, fileID(ref))]
	}
	for _, r := range array(mod["m_RemovedComponents"]) {
		o := target(r)
		if o == nil || o.ClassID == 1 || transform(o.ClassID) {
			return fmt.Errorf("invalid removed component")
		}
		removed[o.FileID] = true
	}
	for _, r := range array(mod["m_RemovedGameObjects"]) {
		o := target(r)
		if o == nil || o.ClassID != 1 {
			return fmt.Errorf("invalid removed GameObject")
		}
		pending := []string{o.FileID}
		for len(pending) > 0 {
			owner := pending[len(pending)-1]
			pending = pending[:len(pending)-1]
			if removed[owner] {
				continue
			}
			removed[owner] = true
			transforms := map[string]bool{}
			for _, c := range instance.objects {
				if fileID(c.Data["m_GameObject"]) == owner {
					removed[c.FileID] = true
					if transform(c.ClassID) {
						transforms[c.FileID] = true
					}
				}
			}
			for _, c := range instance.objects {
				if transform(c.ClassID) && transforms[fileID(c.Data["m_Father"])] {
					pending = append(pending, fileID(c.Data["m_GameObject"]))
				}
			}
		}
	}
	for _, field := range []string{"m_AddedComponents", "m_AddedGameObjects"} {
		for _, v := range array(mod[field]) {
			a := object(v)
			parent := target(a["targetCorrespondingSourceObject"])
			added := byID[fileID(rewrite(a["addedObject"]))]
			if parent == nil || added == nil || removed[parent.FileID] {
				return fmt.Errorf("missing authored prefab addition")
			}
			index := -1
			if a["insertIndex"] != nil {
				index = int(number(a["insertIndex"]))
			}
			if field == "m_AddedComponents" {
				if parent.ClassID != 1 || added.ClassID == 1 || transform(added.ClassID) {
					return fmt.Errorf("invalid added component")
				}
				added.Data["m_GameObject"] = map[string]any{"fileID": parent.FileID}
				items, err := insertReference(array(parent.Data["m_Component"]), map[string]any{"component": map[string]any{"fileID": added.FileID}}, "component", index)
				if err != nil {
					return err
				}
				parent.Data["m_Component"] = items
			} else {
				if !transform(parent.ClassID) || !transform(added.ClassID) {
					return fmt.Errorf("added GameObjects require Transform references")
				}
				if previous := byID[fileID(added.Data["m_Father"])]; previous != nil && previous != parent {
					previous.Data["m_Children"] = filterReferences(array(previous.Data["m_Children"]), "", map[string]bool{added.FileID: true})
				}
				added.Data["m_Father"] = map[string]any{"fileID": parent.FileID}
				items, err := insertReference(array(parent.Data["m_Children"]), map[string]any{"fileID": added.FileID}, "", index)
				if err != nil {
					return err
				}
				parent.Data["m_Children"] = items
			}
		}
	}
	for _, o := range all {
		if removed[o.FileID] {
			o.Removed = true
			continue
		}
		if o.ClassID == 1 {
			o.Data["m_Component"] = filterReferences(array(o.Data["m_Component"]), "component", removed)
		} else if transform(o.ClassID) {
			o.Data["m_Children"] = filterReferences(array(o.Data["m_Children"]), "", removed)
		}
		visitReferences(o.Data, "", func(_ string, r map[string]any) {
			if str(r["guid"]) == "" && removed[fileID(r)] {
				for key := range r {
					delete(r, key)
				}
				r["fileID"] = "0"
			}
		})
	}
	return nil
}

func filterReferences(items []any, key string, removed map[string]bool) []any {
	result := make([]any, 0, len(items))
	for _, v := range items {
		r := v
		if key != "" {
			r = object(v)[key]
		}
		if !removed[fileID(r)] {
			result = append(result, v)
		}
	}
	return result
}
func insertReference(items []any, entry any, key string, index int) ([]any, error) {
	ref := entry
	if key != "" {
		ref = object(entry)[key]
	}
	items = filterReferences(items, key, map[string]bool{fileID(ref): true})
	if index == -1 {
		index = len(items)
	}
	if index < 0 || index > len(items) {
		return nil, fmt.Errorf("insertion index %d exceeds %d", index, len(items))
	}
	result := make([]any, 0, len(items)+1)
	result = append(result, items[:index]...)
	result = append(result, entry)
	result = append(result, items[index:]...)
	return result, nil
}
