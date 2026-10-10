use super::{
    Operation,
    hierarchy::id,
    store::*,
    structural::base_fields,
    yaml_edit::{
        TextAsset, external_reference, items, normalize, protected, reference, visit_references,
    },
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
pub fn native_component_types() -> BTreeMap<i64, &'static str> {
    [
        (20, "Camera"),
        (23, "MeshRenderer"),
        (33, "MeshFilter"),
        (54, "Rigidbody"),
        (64, "MeshCollider"),
        (65, "BoxCollider"),
        (81, "AudioListener"),
        (82, "AudioSource"),
        (95, "Animator"),
        (108, "Light"),
        (120, "LineRenderer"),
        (135, "SphereCollider"),
        (136, "CapsuleCollider"),
        (137, "SkinnedMeshRenderer"),
        (143, "CharacterController"),
        (198, "ParticleSystem"),
        (199, "ParticleSystemRenderer"),
        (205, "LODGroup"),
        (215, "ReflectionProbe"),
        (222, "CanvasRenderer"),
        (223, "Canvas"),
        (225, "CanvasGroup"),
    ]
    .into_iter()
    .collect()
}
impl Store {
    pub(crate) fn rect_transform_fields(
        &self,
        operation: &Operation,
    ) -> Result<BTreeMap<String, Value>> {
        let mut fields = BTreeMap::new();
        if !operation.template_path.is_empty() {
            if operation.template_file_id.is_empty() || operation.template_revision.is_empty() {
                return Err(failure(
                    "invalid_request",
                    "RectTransform template requires fileID and revision",
                    None,
                ));
            }
            let source = self.read_source(&operation.template_path)?;
            if source.revision != operation.template_revision {
                return Err(failure(
                    "revision_conflict",
                    "RectTransform template changed",
                    None,
                ));
            }
            let asset = TextAsset::parse(&source.content)?;
            let document = asset
                .find(&operation.template_file_id)
                .filter(|document| {
                    document.class_id() == 224
                        && !document.stripped()
                        && id(document.field("m_PrefabInstance")) == "0"
                })
                .ok_or_else(|| {
                    failure(
                        "unsupported_component",
                        "template must be an ordinary authored RectTransform",
                        None,
                    )
                })?;
            for (key, value) in document.data() {
                if !protected(key) || key == "serializedVersion" {
                    fields.insert(key.clone(), value.clone());
                }
            }
        }
        for (key, value) in &operation.fields {
            if protected(key) && key != "serializedVersion" {
                return Err(failure(
                    "protected_property",
                    "RectTransform fields cannot replace identity or ownership",
                    None,
                ));
            }
            fields.insert(key.clone(), normalize(value)?);
        }
        for key in [
            "m_LocalPosition",
            "m_LocalRotation",
            "m_LocalScale",
            "m_AnchorMin",
            "m_AnchorMax",
            "m_AnchoredPosition",
            "m_SizeDelta",
            "m_Pivot",
        ] {
            if fields.get(key).is_none_or(Value::is_null) {
                return Err(failure(
                    "invalid_request",
                    "RectTransform creation requires explicit transform/layout fields or a revision-bound template",
                    Some(json!({"property": key})),
                ));
            }
        }
        Ok(fields)
    }
    pub(crate) fn add_native_component(
        &self,
        asset: &mut TextAsset,
        operation_id: &str,
        operation: &Operation,
    ) -> Result<String> {
        if !operation.script_path.is_empty() {
            return Err(failure(
                "invalid_request",
                "native component creation cannot also specify scriptPath",
                None,
            ));
        }
        let type_name = operation
            .component_type
            .strip_prefix("UnityEngine.")
            .unwrap_or(&operation.component_type);
        let types = native_component_types();
        let class_id = if operation.class_id == 0 {
            types
                .iter()
                .find(|(_, name)| **name == type_name)
                .map_or(0, |(class, _)| *class)
        } else {
            operation.class_id
        };
        let unsupported = || {
            failure(
                "unsupported_component",
                "component class/type is outside the pinned RawSceneLoader native profile",
                Some(json!({"classId": class_id, "componentType": operation.component_type})),
            )
        };
        let class = class_id;
        let known = *types.get(&class).ok_or_else(unsupported)?;
        if !type_name.is_empty() && type_name != known {
            return Err(unsupported());
        }
        let (owner, _) = asset.plain_object(&operation.file_id)?;
        let owner_id = asset.docs[owner].id().to_string();
        let mut components = items(asset.docs[owner].field("m_Component")).to_vec();
        let mut data = base_fields();
        if !operation.template_path.is_empty() {
            if operation.template_file_id.is_empty() || operation.template_revision.is_empty() {
                return Err(failure(
                    "invalid_request",
                    "native templates require templateFileId and templateRevision",
                    None,
                ));
            }
            self.asset_path(&operation.template_path, &[".unity", ".prefab"])?;
            let source = self.read_source(&operation.template_path)?;
            if source.revision != operation.template_revision {
                return Err(failure(
                    "revision_conflict",
                    "native component template changed",
                    Some(json!({"path": source.path, "actualRevision": source.revision})),
                ));
            }
            let template = TextAsset::parse(&source.content)?;
            let document = template
                .find(&operation.template_file_id)
                .filter(|document| {
                    document.class_id() == class
                        && !document.stripped()
                        && id(document.field("m_PrefabInstance")) == "0"
                })
                .ok_or_else(|| {
                    failure(
                        "unsupported_component",
                        "template must be an authored ordinary component of the exact selected native class",
                        None,
                    )
                })?;
            for (key, value) in document.data() {
                if protected(key) && key != "serializedVersion" {
                    continue;
                }
                // A same-template local reference cannot silently become a reference
                // to an unrelated target-scene object with the same numeric fileID.
                let local = visit_references(value, &mut |reference| {
                    if id(reference) != "0" && !external_reference(reference) {
                        return Err(failure(
                            "template_local_reference",
                            "template local references must be replaced explicitly through fields",
                            Some(json!({"property": key})),
                        ));
                    }
                    Ok(())
                });
                if !operation.fields.contains_key(key) {
                    local?;
                }
                data.insert(key.clone(), value.clone());
            }
        } else if operation.fields.is_empty() {
            return Err(failure(
                "invalid_request",
                "native creation requires explicit serialized fields or a revision-bound template; default values are not inferred",
                None,
            ));
        }
        for (key, value) in &operation.fields {
            if key.is_empty()
                || key.len() > 256
                || key.contains(['\r', '\n', '\0'])
                || (protected(key) && key != "serializedVersion")
                || key == "m_ObjectHideFlags"
            {
                return Err(failure(
                    "protected_property",
                    "native fields cannot replace identity or ownership",
                    Some(json!({"property": key})),
                ));
            }
            data.insert(key.clone(), normalize(value)?);
        }
        data.insert("m_GameObject".into(), reference(&operation.file_id));
        let component_id = asset.new_id(operation_id, &operation.key);
        asset.append_document(class, &component_id, known, &data)?;
        components.push(json!({"component": reference(&component_id)}));
        asset.set_top_level(&owner_id, "m_Component", &Value::Array(components))?;
        Ok(component_id)
    }
}
