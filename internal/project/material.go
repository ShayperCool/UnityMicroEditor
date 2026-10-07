package goproject

import (
	"fmt"
	"path/filepath"
)

// A .mat already contains authored runtime properties. Its compiled Shader and
// imported textures are resolved separately; no shader compiler is substituted.
func (s *Session) refreshAuthoredMaterial(asset map[string]any) error {
	path := str(asset["path"])
	physical, err := s.source(path)
	if err != nil {
		return err
	}
	if err := s.depend(path); err != nil {
		return err
	}
	documents, err := s.parse(physical)
	if err != nil {
		return err
	}
	var material *Document
	for index := range documents {
		doc := &documents[index]
		if doc.ClassID == 21 {
			if material != nil {
				return fmt.Errorf("material %s has multiple runtime objects", path)
			}
			material = doc
		} else if doc.ClassID != 114 || str(doc.Data["m_EditorClassIdentifier"]) != "Unity.RenderPipelines.Universal.Editor::UnityEditor.Rendering.Universal.AssetVersion" {
			return fmt.Errorf("material %s has unsupported subasset class %d", path, doc.ClassID)
		}
	}
	if material == nil {
		return fmt.Errorf("material %s has no class21 document", path)
	}
	data := material.Data
	if number(data["serializedVersion"]) != 8 {
		return fmt.Errorf("material %s requires serializedVersion8", path)
	}
	if fileID(data["m_Shader"]) == "0" {
		return fmt.Errorf("material %s has no Shader", path)
	}
	if err := s.validateMaterialParents(path, material, map[string]bool{}); err != nil {
		return err
	}
	for _, group := range []string{"m_TexEnvs", "m_Ints", "m_Floats", "m_Colors"} {
		for _, item := range array(object(data["m_SavedProperties"])[group]) {
			if len(object(item)) != 1 {
				return fmt.Errorf("material %s has malformed %s property", path, group)
			}
		}
	}
	asset["fileId"], asset["classId"], asset["typeName"] = material.FileID, 21, "UnityEngine.Material"
	asset["name"] = str(data["m_Name"])
	asset["dataJson"], err = compactJSON(data)
	asset["references"] = referenceList(data)
	return err
}

// Variant header defaults are not overrides when the serialized mask is zero.
// Runtime inheritance copies the parent material and applies m_SavedProperties.
// Nonzero header masks need a separately verified serialized-property mapping.
func (s *Session) validateMaterialParents(path string, material *Document, visiting map[string]bool) error {
	if visiting[path] {
		return fmt.Errorf("material parent cycle at %s", path)
	}
	visiting[path] = true
	defer delete(visiting, path)
	data := material.Data
	if fileID(data["m_Parent"]) == "0" {
		return nil
	}
	if str(data["m_ModifiedSerializedProperties"]) != "0" {
		return fmt.Errorf("material variant %s has unsupported modified serialized properties", path)
	}
	parentReference := object(data["m_Parent"])
	parentGUID := str(parentReference["guid"])
	if !validNativeGUID(parentGUID) {
		return fmt.Errorf("material variant %s has invalid parent GUID", path)
	}
	parentPath := s.byGUID[parentGUID]
	if parentPath == "" || filepath.Ext(parentPath) != ".mat" {
		// Imported parents can be subassets of a native model. Their Material
		// and Shader identities are checked through public runtime objects.
		return nil
	}
	if err := s.depend(parentPath); err != nil {
		return err
	}
	physical, err := s.source(parentPath)
	if err != nil {
		return err
	}
	documents, err := s.parse(physical)
	if err != nil {
		return err
	}
	var parent *Document
	for index := range documents {
		document := &documents[index]
		if document.ClassID == 21 && document.FileID == fileID(parentReference) {
			if parent != nil {
				return fmt.Errorf("material variant %s has ambiguous parent identity", path)
			}
			parent = document
		}
	}
	if parent == nil {
		return fmt.Errorf("material variant %s has no class21 parent %s", path, parentPath)
	}
	shader, parentShader := object(data["m_Shader"]), object(parent.Data["m_Shader"])
	if fileID(shader) != fileID(parentShader) || str(shader["guid"]) != str(parentShader["guid"]) {
		return fmt.Errorf("material variant %s has a different Shader from its parent", path)
	}
	return s.validateMaterialParents(parentPath, parent, visiting)
}
