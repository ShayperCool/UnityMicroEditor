package goproject

import (
	"fmt"
	"path/filepath"
	"sort"
	"strings"
)

var supportedClasses = map[int]bool{1: true, 4: true, 20: true, 23: true, 33: true, 54: true, 65: true, 108: true, 114: true, 135: true, 136: true, 104: true, 64: true, 81: true, 82: true, 95: true, 120: true, 137: true, 143: true, 198: true, 199: true, 205: true, 215: true, 222: true, 223: true, 224: true, 225: true, 29: true, 157: true, 196: true, 1660057539: true}

func visitReferences(value any, path string, visit func(string, map[string]any)) {
	switch v := value.(type) {
	case map[string]any:
		if _, exists := v["fileID"]; exists {
			visit(path, v)
			return
		}
		keys := make([]string, 0, len(v))
		for k := range v {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		for _, k := range keys {
			next := k
			if path != "" {
				next = path + "." + k
			}
			visitReferences(v[k], next, visit)
		}
	case []any:
		for i, item := range v {
			visitReferences(item, fmt.Sprintf("%s[%d]", path, i), visit)
		}
	}
}

func referenceList(data any) []any {
	result := []any{}
	visitReferences(data, "", func(property string, r map[string]any) {
		result = append(result, map[string]any{"property": property, "fileId": fileID(r), "guid": str(r["guid"]), "type": number(r["type"])})
	})
	return result
}

func scalarData(value any) any {
	switch v := value.(type) {
	case map[string]any:
		if _, reference := v["fileID"]; reference {
			return nil
		}
		result := map[string]any{}
		for k, item := range v {
			if r, ok := item.(map[string]any); ok {
				if _, reference := r["fileID"]; reference {
					continue
				}
			}
			result[k] = scalarData(item)
		}
		return result
	case []any:
		result := make([]any, len(v))
		for i, item := range v {
			result[i] = scalarData(item)
		}
		return result
	default:
		return value
	}
}

func (s *Session) manifestObjects(objects []*expandedObject) ([]any, error) {
	result := make([]any, 0, len(objects))
	for _, o := range objects {
		data := o.Data
		if o.ClassID == 20 {
			aliases := map[string]string{"near clip plane": "nearClipPlane", "far clip plane": "farClipPlane", "field of view": "fieldOfView", "orthographic size": "orthographicSize", "m_projectionMatrixMode": "ProjectionMatrixMode"}
			data = map[string]any{}
			for k, v := range o.Data {
				name := aliases[k]
				if name == "" {
					name = k
				}
				data[name] = v
			}
		}
		typeName := o.TypeName
		scalar := data
		if o.ClassID == 114 {
			r := object(data["m_Script"])
			guid := str(r["guid"])
			typeName = s.scriptTypes[guid]
			if guid == "0000000000000000e000000000000000" && fileID(r) == "19102" {
				typeName = "UnityEngine.UIElements.UIDocument"
			}
			if typeName == "" && !o.MissingScript {
				return nil, fmt.Errorf("unresolved managed component type %s in %s", guid, o.SourcePath)
			}
			var err error
			data, err = derivedNetworkData(o, typeName, data)
			if err != nil {
				return nil, err
			}
			scalar = map[string]any{}
			metadata := map[string]bool{"m_ObjectHideFlags": true, "m_GameObject": true, "m_Enabled": true, "m_EditorHideFlags": true, "m_Script": true, "m_Name": true, "m_EditorClassIdentifier": true}
			for k, v := range data {
				if !metadata[k] {
					scalar[k] = v
				}
			}
		}
		full, err := compactJSON(data)
		if err != nil {
			return nil, err
		}
		scalars, err := compactJSON(scalarData(scalar))
		if err != nil {
			return nil, err
		}
		result = append(result, map[string]any{"fileId": o.FileID, "classId": o.ClassID, "gameObjectId": fileID(data["m_GameObject"]), "sourcePath": o.SourcePath, "guid": o.GUID, "typeName": typeName, "skipMissingScript": o.MissingScript, "dataJson": full, "scalarDataJson": scalars, "references": referenceList(data)})
	}
	return result, nil
}

func orderSceneRoots(objects []any) ([]any, error) {
	ranks := map[string]int{}
	owners := map[string]string{}
	metadata := 0
	for _, v := range objects {
		o := object(v)
		if !transform(int(number(o["classId"]))) {
			continue
		}
		var data map[string]any
		if err := jsonDecodeString(str(o["dataJson"]), &data); err != nil {
			return nil, err
		}
		if fileID(data["m_Father"]) == "0" {
			owners[str(o["fileId"])] = fileID(data["m_GameObject"])
		}
	}
	for _, v := range objects {
		o := object(v)
		if number(o["classId"]) != 1660057539 {
			continue
		}
		metadata++
		data := map[string]any{}
		_ = jsonDecodeString(str(o["dataJson"]), &data)
		for i, r := range array(data["m_Roots"]) {
			if str(object(r)["guid"]) != "" {
				return nil, fmt.Errorf("SceneRoots requires local authored references")
			}
			id := fileID(r)
			if _, exists := ranks[id]; exists {
				return nil, fmt.Errorf("SceneRoots contains duplicate %s", id)
			}
			if owners[id] == "" {
				return nil, fmt.Errorf("SceneRoots contains non-root %s", id)
			}
			ranks[id] = i
		}
	}
	if metadata > 1 {
		return nil, fmt.Errorf("scene has more than one SceneRoots document")
	}
	if metadata == 1 && len(ranks) != len(owners) {
		return nil, fmt.Errorf("SceneRoots must list every root Transform")
	}
	if len(ranks) == 0 {
		return objects, nil
	}
	ownerRanks := map[string]int{}
	for id, rank := range ranks {
		ownerRanks[owners[id]] = rank
	}
	rank := func(o map[string]any) (int, int) {
		id := str(o["fileId"])
		if number(o["classId"]) == 1 {
			if r, ok := ownerRanks[id]; ok {
				return r, 0
			}
		} else if transform(int(number(o["classId"]))) {
			if r, ok := ranks[id]; ok {
				return r, 1
			}
		}
		return len(ranks), 2
	}
	sort.SliceStable(objects, func(i, j int) bool {
		a, ak := rank(object(objects[i]))
		b, bk := rank(object(objects[j]))
		if a != b {
			return a < b
		}
		return ak < bk
	})
	return objects, nil
}

func (s *Session) validate(objects []*expandedObject) error {
	byID := map[string]*expandedObject{}
	for _, o := range objects {
		if byID[o.FileID] != nil {
			return fmt.Errorf("duplicate expanded identity %s", o.FileID)
		}
		byID[o.FileID] = o
		if !supportedClasses[o.ClassID] {
			return fmt.Errorf("class %d %s has no runtime adapter", o.ClassID, o.TypeName)
		}
	}
	for _, o := range objects {
		var refError error
		visitReferences(o.Data, "", func(property string, r map[string]any) {
			if refError != nil {
				return
			}
			id, guid := fileID(r), str(r["guid"])
			if id == "0" || builtinGUID(guid) || o.MissingScript && property == "m_Script" {
				return
			}
			if guid != "" {
				if s.byGUID[guid] == "" {
					refError = fmt.Errorf("unresolved GUID %s at %s.%s", guid, o.FileID, property)
				}
			} else if byID[id] == nil {
				refError = fmt.Errorf("dangling scene reference %s at %s.%s", id, o.FileID, property)
			}
		})
		if refError != nil {
			return refError
		}
		if o.ClassID != 1 && o.ClassID != 104 && o.ClassID != 29 && o.ClassID != 157 && o.ClassID != 196 && o.ClassID != 1660057539 {
			owner := byID[fileID(o.Data["m_GameObject"])]
			if owner == nil || owner.ClassID != 1 {
				return fmt.Errorf("component %s requires a GameObject", o.FileID)
			}
		}
		if o.ClassID == 1 {
			transforms := 0
			for _, v := range array(o.Data["m_Component"]) {
				id := fileID(object(v)["component"])
				c := byID[id]
				if c == nil {
					return fmt.Errorf("GameObject %s component %s is missing", o.FileID, id)
				}
				if transform(c.ClassID) {
					transforms++
				}
				if fileID(c.Data["m_GameObject"]) != o.FileID {
					return fmt.Errorf("component %s has a different GameObject owner", id)
				}
			}
			if transforms != 1 {
				return fmt.Errorf("GameObject %s requires exactly one Transform", o.FileID)
			}
		}
		if transform(o.ClassID) {
			seen := map[string]bool{}
			current := o
			for current != nil {
				if seen[current.FileID] {
					return fmt.Errorf("Transform cycle at %s", o.FileID)
				}
				seen[current.FileID] = true
				parent := fileID(current.Data["m_Father"])
				if parent == "0" {
					break
				}
				current = byID[parent]
				if current != nil && !transform(current.ClassID) {
					return fmt.Errorf("Transform %s parent is not a Transform", o.FileID)
				}
			}
		}
	}
	return nil
}

func (s *Session) prepareAssets(sceneObjects []*expandedObject) (*assetPreparation, error) {
	bindings := s.importedBindingClosure()
	assets := map[string]map[string]any{}
	for key, a := range s.assets {
		assets[key] = clone(a).(map[string]any)
	}
	catalogAssets := map[string]map[string]any{}
	for _, a := range s.catalog.Assets {
		catalogAssets[assetKey(str(a["guid"]), str(a["fileId"]))] = a
	}
	pending := newAssetReferenceWorklist()
	for _, collection := range []string{"preloadedAssets", "renderPipelineGlobalSettings"} {
		for _, item := range array(s.baseline[collection]) {
			value := object(item)
			if err := pending.addIdentity(str(value["guid"]), str(value["fileId"])); err != nil {
				return nil, err
			}
		}
	}
	if err := pending.addIdentity(str(s.baseline["renderPipelineGuid"]), str(s.baseline["renderPipelineFileId"])); err != nil {
		return nil, err
	}
	for _, path := range s.extraAssets {
		guid := s.byPath[path]
		if guid == "" {
			return nil, fmt.Errorf("runtime-addressed prefab has no source GUID: %s", path)
		}
		objects, _, err := s.expand(path, "", nil)
		if err != nil {
			return nil, err
		}
		root := ""
		for _, value := range objects {
			if transform(value.ClassID) && fileID(value.Data["m_Father"]) == "0" {
				if root != "" {
					return nil, fmt.Errorf("runtime-addressed prefab needs exactly one root: %s", path)
				}
				root = fileID(value.Data["m_GameObject"])
			}
		}
		if root == "" || root == "0" {
			return nil, fmt.Errorf("runtime-addressed prefab has no root: %s", path)
		}
		if err := pending.addIdentity(guid, root); err != nil {
			return nil, err
		}
	}
	queue := func(data any) error {
		var queueErr error
		visitReferences(data, "", func(_ string, r map[string]any) {
			if queueErr == nil {
				queueErr = pending.add(r)
			}
		})
		return queueErr
	}
	for _, o := range sceneObjects {
		if o.MissingScript {
			data := clone(o.Data).(map[string]any)
			delete(data, "m_Script")
			if err := queue(data); err != nil {
				return nil, err
			}
		} else {
			if err := queue(o.Data); err != nil {
				return nil, err
			}
		}
	}
	initialAssetKeys := make([]string, 0, len(assets))
	for key := range assets {
		initialAssetKeys = append(initialAssetKeys, key)
	}
	sort.Strings(initialAssetKeys)
	for _, key := range initialAssetKeys {
		a := assets[key]
		if err := s.refreshAsset(a); err != nil {
			return nil, err
		}
		currentKey := assetKey(str(a["guid"]), str(a["fileId"]))
		if currentKey != key {
			if assets[currentKey] != nil {
				return nil, fmt.Errorf("refreshed asset identity collides with another descriptor: %s", currentKey)
			}
			delete(assets, key)
			assets[currentKey] = a
		}
		if err := bindings.queue(a, pending); err != nil {
			return nil, err
		}
		importedReferences, err := s.importedSourceReferences(a, catalogAssets)
		if err != nil {
			return nil, err
		}
		if err := pending.addAll(importedReferences); err != nil {
			return nil, err
		}
		for _, r := range array(a["references"]) {
			v := object(r)
			if err := pending.addIdentity(str(v["guid"]), str(v["fileId"])); err != nil {
				return nil, err
			}
		}
		for _, v := range array(a["objects"]) {
			for _, r := range array(object(v)["references"]) {
				ref := object(r)
				if object(v)["skipMissingScript"] == true && str(ref["property"]) == "m_Script" {
					continue
				}
				if err := pending.addIdentity(str(ref["guid"]), str(ref["fileId"])); err != nil {
					return nil, err
				}
			}
		}
	}
	for {
		r, exists := pending.pop()
		if !exists {
			break
		}
		guid, id := r.guid, r.fileID
		key := assetKey(guid, id)
		if assets[key] != nil {
			continue
		}
		knownPrefab := false
		for _, a := range assets {
			if str(a["guid"]) == guid && str(a["kind"]) == "prefab" {
				for _, alias := range array(a["objectAliases"]) {
					if str(object(alias)["fileId"]) == id {
						knownPrefab = true
						break
					}
				}
				for _, o := range array(a["objects"]) {
					if str(object(o)["fileId"]) == id {
						knownPrefab = true
						break
					}
				}
			}
		}
		if knownPrefab {
			continue
		}
		if native := catalogAssets[key]; native != nil {
			a := clone(native).(map[string]any)
			a["kind"] = "imported"
			assets[key] = a
			if err := s.depend(str(a["path"])); err != nil {
				return nil, err
			}
			if err := bindings.queue(a, pending); err != nil {
				return nil, err
			}
			importedReferences, err := s.importedSourceReferences(a, catalogAssets)
			if err != nil {
				return nil, err
			}
			if err := pending.addAll(importedReferences); err != nil {
				return nil, err
			}
			continue
		}
		path := s.byGUID[guid]
		if path == "" {
			return nil, fmt.Errorf("asset GUID %s is outside known closure", guid)
		}
		for _, existing := range assets {
			if str(existing["guid"]) == guid && str(existing["kind"]) != "imported" && !(strings.ToLower(filepath.Ext(path)) == ".asset" && str(existing["kind"]) == "scriptableObject") {
				return nil, fmt.Errorf("raw asset %s has no object %s", path, id)
			}
		}
		ext := strings.ToLower(filepath.Ext(path))
		name := strings.TrimSuffix(filepath.Base(path), filepath.Ext(path))
		a := map[string]any{"guid": guid, "fileId": id, "path": path, "name": name, "references": []any{}, "dataJson": "{}", "importerJson": "{}"}
		switch ext {
		case ".cs":
			typeName := s.scriptTypes[guid]
			if typeName == "" {
				return nil, fmt.Errorf("new script %s requires compilation type metadata", path)
			}
			a["kind"] = "script"
			a["classId"] = 115
			a["typeName"] = typeName
			a["executionOrder"] = 0
		case ".prefab":
			a["kind"] = "prefab"
		case ".asset":
			a["kind"] = "scriptableObject"
		case ".mat":
			a["kind"] = "authoredMaterial"
		case ".png", ".jpg", ".jpeg":
			a["kind"] = "texture"
		case ".cubemap":
			a["kind"] = "cubemap"
		default:
			return nil, &NeedsImportError{path}
		}
		if err := s.refreshAsset(a); err != nil {
			return nil, err
		}
		validID := str(a["fileId"]) == id
		if str(a["kind"]) == "prefab" {
			for _, alias := range array(a["objectAliases"]) {
				if str(object(alias)["fileId"]) == id {
					validID = true
				}
			}
			for _, o := range array(a["objects"]) {
				if str(object(o)["fileId"]) == id {
					validID = true
				}
			}
		}
		if !validID {
			return nil, fmt.Errorf("raw asset %s has no object %s", path, id)
		}
		assets[assetKey(guid, str(a["fileId"]))] = a
		for _, r := range array(a["references"]) {
			ref := object(r)
			if err := pending.addIdentity(str(ref["guid"]), str(ref["fileId"])); err != nil {
				return nil, err
			}
		}
		for _, v := range array(a["objects"]) {
			for _, r := range array(object(v)["references"]) {
				ref := object(r)
				if object(v)["skipMissingScript"] == true && str(ref["property"]) == "m_Script" {
					continue
				}
				if err := pending.addIdentity(str(ref["guid"]), str(ref["fileId"])); err != nil {
					return nil, err
				}
			}
		}
	}
	list := make([]any, 0, len(assets))
	for _, a := range assets {
		list = append(list, a)
	}
	sort.Slice(list, func(i, j int) bool {
		a, b := object(list[i]), object(list[j])
		ag, bg := str(a["guid"]), str(b["guid"])
		if ag != bg {
			return ag < bg
		}
		return str(a["fileId"]) < str(b["fileId"])
	})
	selectedBindings, err := bindings.result(assets)
	if err != nil {
		return nil, err
	}
	return &assetPreparation{Assets: list, ImportedBindings: selectedBindings}, nil
}

func (s *Session) refreshAsset(a map[string]any) error {
	// Runtime prefab descriptors and custom ScriptableObjects remain raw and are
	// refreshed as authored YAML. Imported descriptors remain immutable.
	path, kind := str(a["path"]), str(a["kind"])
	if kind == "authoredMaterial" {
		return s.refreshAuthoredMaterial(a)
	} else if kind == "texture" {
		return s.refreshAuthoredTexture(a)
	} else if kind == "cubemap" {
		return s.refreshAuthoredCubemap(a)
	} else if kind == "script" {
		if typeName := s.scriptTypes[str(a["guid"])]; typeName != "" {
			a["typeName"] = typeName
		}
	} else if kind == "prefab" {
		objects, aliases, err := s.expand(path, "", nil)
		if err != nil {
			return err
		}
		if err := s.validate(objects); err != nil {
			return err
		}
		roots := []*expandedObject{}
		for _, o := range objects {
			if transform(o.ClassID) && fileID(o.Data["m_Father"]) == "0" {
				roots = append(roots, o)
			}
		}
		if len(roots) != 1 {
			return fmt.Errorf("runtime prefab %s needs one root", path)
		}
		root := fileID(roots[0].Data["m_GameObject"])
		a["fileId"] = root
		a["rootFileId"] = root
		a["classId"] = 1
		converted, err := s.manifestObjects(objects)
		if err != nil {
			return err
		}
		a["objects"] = converted
		keys := make([]string, 0, len(aliases))
		for k := range aliases {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		out := []any{}
		for _, k := range keys {
			out = append(out, map[string]any{"fileId": k, "objectId": aliases[k]})
		}
		a["objectAliases"] = out
	} else if kind == "scriptableObject" || kind == "mesh" {
		physical, err := s.source(path)
		if err != nil {
			return err
		}
		if err := s.depend(path); err != nil {
			return err
		}
		docs, err := s.parse(physical)
		if err != nil {
			return err
		}
		if len(docs) == 1 && docs[0].ClassID == 43 {
			if err := validateAuthoredMesh(docs[0].Data); err != nil {
				return fmt.Errorf("raw Mesh %s: %w", path, err)
			}
			a["kind"], a["classId"], a["typeName"] = "mesh", 43, "UnityEngine.Mesh"
			a["fileId"], a["name"] = docs[0].FileID, str(docs[0].Data["m_Name"])
			a["dataJson"], err = compactJSON(docs[0].Data)
			a["references"] = referenceList(docs[0].Data)
			return err
		}
		if kind == "mesh" {
			return fmt.Errorf("raw Mesh %s changed its source object type", path)
		}
		return s.refreshScriptableObject(a, docs)
	}
	return nil
}
