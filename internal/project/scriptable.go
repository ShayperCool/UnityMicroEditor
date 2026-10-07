package goproject

import "fmt"

func (s *Session) refreshScriptableObject(asset map[string]any, documents []Document) error {
	path, guid, id := str(asset["path"]), str(asset["guid"]), str(asset["fileId"])
	known := make(map[string]bool, len(documents))
	var selected *Document
	for index := range documents {
		document := &documents[index]
		if document.ClassID != 114 {
			return fmt.Errorf("raw ScriptableObject %s has unsupported class%d subasset", path, document.ClassID)
		}
		known[document.FileID] = true
		if document.FileID == id || len(documents) == 1 && id == "" {
			selected = document
		}
	}
	if selected == nil {
		return fmt.Errorf("raw ScriptableObject %s has no source localID %s", path, id)
	}
	data := selected.Data
	var referencesError error
	visitReferences(data, "", func(property string, reference map[string]any) {
		localID := fileID(reference)
		if localID == "0" || str(reference["guid"]) != "" {
			return
		}
		if !known[localID] {
			referencesError = fmt.Errorf("ScriptableObject %s.%s has unknown same-file localID %s", path, property, localID)
			return
		}
		reference["guid"] = guid
	})
	if referencesError != nil {
		return referencesError
	}
	script := object(data["m_Script"])
	typeName := s.scriptTypes[str(script["guid"])]
	if str(script["guid"]) == "0000000000000000e000000000000000" {
		// Pinned Unity6's serialized PanelSettings identity, independently
		// recorded in this project's source-authored UI/Net panel assets.
		if fileID(script) != "19101" || str(data["m_EditorClassIdentifier"]) != "UnityEngine.dll::UnityEngine.UIElements.PanelSettings" {
			return fmt.Errorf("raw ScriptableObject %s has an unverified built-in MonoScript identity", path)
		}
		typeName = "UnityEngine.UIElements.PanelSettings"
	}
	if typeName == "" {
		return fmt.Errorf("raw ScriptableObject %s requires type metadata for MonoScript %s/%s", path, str(script["guid"]), fileID(script))
	}
	asset["fileId"], asset["classId"], asset["typeName"], asset["name"] = selected.FileID, 114, typeName, str(data["m_Name"])
	var err error
	asset["dataJson"], err = compactJSON(data)
	asset["references"] = referenceList(data)
	return err
}
