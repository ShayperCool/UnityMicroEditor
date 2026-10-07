package goproject

import (
	"bytes"
	"fmt"
	"path/filepath"
	"strings"
)

// Imported ScriptableObjects retain their native representation. Their authored
// PPtrs still define the dependency closure, including prefab pointers omitted
// from native packs and signed same-file subasset identities.
func (s *Session) importedSourceReferences(asset map[string]any, nativeAssets map[string]map[string]any) ([]map[string]any, error) {
	if str(asset["kind"]) != "imported" || number(asset["classId"]) != 114 || !strings.EqualFold(filepath.Ext(str(asset["path"])), ".asset") {
		return nil, nil
	}
	path, guid, id := str(asset["path"]), str(asset["guid"]), str(asset["fileId"])
	native := nativeAssets[assetKey(guid, id)]
	if !validNativeGUID(guid) || !validNativeID(id) || s.byGUID[guid] != path || native == nil ||
		number(native["classId"]) != 114 || str(native["path"]) != path || str(native["typeName"]) != str(asset["typeName"]) {
		return nil, fmt.Errorf("imported ScriptableObject source identity is invalid: %s/%s (%s)", guid, id, path)
	}
	physical, err := s.source(path)
	if err != nil {
		return nil, err
	}
	content, sourceSHA, err := s.read(physical, true)
	if err != nil {
		return nil, err
	}
	metaPath, err := s.source(path + ".meta")
	if err != nil {
		return nil, err
	}
	meta, metaSHA, err := s.read(metaPath, true)
	if err != nil {
		return nil, err
	}
	match := guidLine.FindSubmatch(meta)
	if !s.nativeCatalogSourceMatches(path, sourceSHA) || !s.nativeCatalogSourceMatches(path+".meta", metaSHA) ||
		match == nil || strings.ToLower(string(match[1])) != guid {
		return nil, &NeedsImportError{Path: path}
	}
	trimmed := bytes.TrimSpace(bytes.TrimPrefix(content, []byte{0xef, 0xbb, 0xbf}))
	if !bytes.HasPrefix(trimmed, []byte("%YAML")) && !bytes.HasPrefix(trimmed, []byte("--- !u!")) {
		// Opaque native binary data has no authored YAML PPtrs to traverse.
		return nil, nil
	}
	documents, err := s.parse(physical)
	if err != nil {
		return nil, err
	}
	known := map[string]bool{}
	var selected *Document
	for index := range documents {
		document := &documents[index]
		known[document.FileID] = true
		if document.FileID == id {
			selected = document
		}
	}
	if selected == nil || selected.ClassID != 114 || selected.Stripped {
		return nil, fmt.Errorf("imported ScriptableObject source has no exact class114 object: %s/%s (%s)", guid, id, path)
	}
	references := []map[string]any{}
	var referenceError error
	visitReferences(selected.Data, "", func(property string, reference map[string]any) {
		if referenceError != nil {
			return
		}
		localID := fileID(reference)
		if localID == "0" || builtinGUID(str(reference["guid"])) {
			return
		}
		value := clone(reference).(map[string]any)
		if str(value["guid"]) == "" {
			if !known[localID] {
				referenceError = fmt.Errorf("imported ScriptableObject %s.%s has unknown same-file localID %s", path, property, localID)
				return
			}
			value["guid"] = guid
		}
		targetGUID := str(value["guid"])
		target := nativeAssets[assetKey(targetGUID, localID)]
		// Native packs already retain shaders, meshes, textures and other opaque
		// references. Only ScriptableObject callbacks and raw prefab references
		// need to extend this publication closure. Obsolete importer/debug PPtrs
		// are not reinterpreted as live authored object dependencies.
		if (target == nil || number(target["classId"]) != 114) && !strings.EqualFold(filepath.Ext(s.byGUID[targetGUID]), ".prefab") {
			return
		}
		if len(references) == 32768 {
			referenceError = fmt.Errorf("imported ScriptableObject reference closure exceeds supported bound: %s/%s", guid, id)
			return
		}
		references = append(references, value)
	})
	return references, referenceError
}
