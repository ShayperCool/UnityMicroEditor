package authoring

import "strings"

// These names/classes are the public built-ins accepted by RawSceneLoader and
// ImportedAssetRuntimeComponents for the pinned Unity profile. Creation never
// fabricates their type-specific defaults: callers supply fields or an existing
// revision-bound authored template.
var nativeComponentTypes = map[int]string{
	20: "Camera", 23: "MeshRenderer", 33: "MeshFilter", 54: "Rigidbody",
	64: "MeshCollider", 65: "BoxCollider", 81: "AudioListener", 82: "AudioSource",
	95: "Animator", 108: "Light", 120: "LineRenderer", 135: "SphereCollider",
	136: "CapsuleCollider", 137: "SkinnedMeshRenderer", 143: "CharacterController",
	198: "ParticleSystem", 199: "ParticleSystemRenderer", 205: "LODGroup",
	215: "ReflectionProbe", 222: "CanvasRenderer", 223: "Canvas", 225: "CanvasGroup",
}

func NativeComponentTypes() map[int]string {
	result := make(map[int]string, len(nativeComponentTypes))
	for id, name := range nativeComponentTypes {
		result[id] = name
	}
	return result
}

func (s *Store) addNativeComponent(asset *yamlAsset, operationID string, operation Operation) (string, error) {
	if operation.ScriptPath != "" {
		return "", failure("invalid_request", "native component creation cannot also specify scriptPath", nil)
	}
	classID, typeName := operation.ClassID, strings.TrimPrefix(operation.ComponentType, "UnityEngine.")
	if classID == 0 {
		for candidate, name := range nativeComponentTypes {
			if name == typeName {
				classID = candidate
				break
			}
		}
	}
	known := nativeComponentTypes[classID]
	if known == "" || typeName != "" && typeName != known {
		return "", failure("unsupported_component", "component class/type is outside the pinned RawSceneLoader native profile", map[string]any{"classId": classID, "componentType": operation.ComponentType})
	}
	owner, _, err := asset.plainObject(operation.FileID)
	if err != nil {
		return "", err
	}
	data := baseFields()
	if operation.TemplatePath != "" {
		if operation.TemplateFileID == "" || operation.TemplateRevision == "" {
			return "", failure("invalid_request", "native templates require templateFileId and templateRevision", nil)
		}
		if _, err := s.assetPath(operation.TemplatePath, ".unity", ".prefab"); err != nil {
			return "", err
		}
		source, err := s.readSource(operation.TemplatePath)
		if err != nil {
			return "", err
		}
		if source.Revision != operation.TemplateRevision {
			return "", failure("revision_conflict", "native component template changed", map[string]any{"path": source.Path, "actualRevision": source.Revision})
		}
		template, err := parseAsset([]byte(source.Content))
		if err != nil {
			return "", err
		}
		document := template.find(operation.TemplateFileID)
		if document == nil || document.classID != classID || document.stripped || idReference(document.data["m_PrefabInstance"]) != "0" {
			return "", failure("unsupported_component", "template must be an authored ordinary component of the exact selected native class", nil)
		}
		for key, value := range document.data {
			if protectedProperty(key) && key != "serializedVersion" {
				continue
			}
			// A same-template local reference cannot silently become a reference
			// to an unrelated target-scene object with the same numeric fileID.
			if err := walkReferences(value, func(ref map[string]any) error {
				if idReference(ref) != "0" && !externalReference(ref) && operation.TemplatePath != "" {
					return failure("template_local_reference", "template local references must be replaced explicitly through fields", map[string]any{"property": key})
				}
				return nil
			}); err != nil {
				if _, replaced := operation.Fields[key]; !replaced {
					return "", err
				}
			}
			data[key] = value
		}
	} else if len(operation.Fields) == 0 {
		return "", failure("invalid_request", "native creation requires explicit serialized fields or a revision-bound template; default values are not inferred", nil)
	}
	for key, value := range operation.Fields {
		if key == "" || len(key) > 256 || strings.ContainsAny(key, "\r\n\x00") || protectedProperty(key) && key != "serializedVersion" || key == "m_ObjectHideFlags" {
			return "", failure("protected_property", "native fields cannot replace identity or ownership", map[string]any{"property": key})
		}
		normalized, err := normalizeJSON(value)
		if err != nil {
			return "", err
		}
		data[key] = normalized
	}
	data["m_GameObject"] = reference(operation.FileID)
	id := asset.newID(operationID, operation.Key)
	if err := asset.appendDocument(classID, id, known, data); err != nil {
		return "", err
	}
	components, _ := owner.data["m_Component"].([]any)
	components = append(append([]any{}, components...), map[string]any{"component": reference(id)})
	if err := asset.setTopLevel(owner.id, "m_Component", components); err != nil {
		return "", err
	}
	return id, nil
}

func (s *Store) rectTransformFields(operation Operation) (map[string]any, error) {
	fields := map[string]any{}
	if operation.TemplatePath != "" {
		if operation.TemplateFileID == "" || operation.TemplateRevision == "" {
			return nil, failure("invalid_request", "RectTransform template requires fileID and revision", nil)
		}
		source, err := s.readSource(operation.TemplatePath)
		if err != nil {
			return nil, err
		}
		if source.Revision != operation.TemplateRevision {
			return nil, failure("revision_conflict", "RectTransform template changed", nil)
		}
		asset, err := parseAsset([]byte(source.Content))
		if err != nil {
			return nil, err
		}
		document := asset.find(operation.TemplateFileID)
		if document == nil || document.classID != 224 || document.stripped || idReference(document.data["m_PrefabInstance"]) != "0" {
			return nil, failure("unsupported_component", "template must be an ordinary authored RectTransform", nil)
		}
		for key, value := range document.data {
			if !protectedProperty(key) || key == "serializedVersion" {
				fields[key] = value
			}
		}
	}
	for key, value := range operation.Fields {
		if protectedProperty(key) && key != "serializedVersion" {
			return nil, failure("protected_property", "RectTransform fields cannot replace identity or ownership", nil)
		}
		normalized, err := normalizeJSON(value)
		if err != nil {
			return nil, err
		}
		fields[key] = normalized
	}
	for _, key := range []string{"m_LocalPosition", "m_LocalRotation", "m_LocalScale", "m_AnchorMin", "m_AnchorMax", "m_AnchoredPosition", "m_SizeDelta", "m_Pivot"} {
		if fields[key] == nil {
			return nil, failure("invalid_request", "RectTransform creation requires explicit transform/layout fields or a revision-bound template", map[string]any{"property": key})
		}
	}
	return fields, nil
}
