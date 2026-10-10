use super::{
    Operation,
    hierarchy::{id, sprint},
    prefab_linking::prefab_graph_identity,
    prefab_serialization::set_prefab_modification_field,
    store::*,
    yaml_edit::{TextAsset, items, normalize, normalize_typed, protected, reference},
};
use crate::graph::{GraphObject, apply_override};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, fs};
pub(crate) fn check_graph_revisions(
    revisions: &BTreeMap<String, String>,
    expected: &BTreeMap<String, String>,
) -> Result<()> {
    for (path, actual) in revisions {
        match expected.get(path).filter(|revision| !revision.is_empty()) {
            None => {
                return Err(failure(
                    "revision_required",
                    "prefab operation requires each dependency revision from resolved hierarchy",
                    Some(json!({"path": path})),
                ));
            }
            Some(revision) if revision != actual => {
                return Err(failure(
                    "revision_conflict",
                    "prefab authoring dependency changed",
                    Some(
                        json!({"path": path, "actualRevision": actual, "expectedRevision": revision}),
                    ),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}
pub(crate) fn source_prefab_guid(instance: &Map<String, Value>) -> String {
    instance
        .get("m_SourcePrefab")
        .and_then(|reference| reference["guid"].as_str())
        .unwrap_or_default()
        .to_string()
}
fn instance_record(guid: &str, parent: &str, modifications: Vec<Value>) -> Map<String, Value> {
    json!({"m_ObjectHideFlags": 0, "serializedVersion": 2,
        "m_SourcePrefab": {"fileID": "100100000", "guid": guid, "type": 3},
        "m_Modification": {"serializedVersion": 3, "m_TransformParent": reference(parent),
            "m_Modifications": modifications, "m_RemovedComponents": [], "m_RemovedGameObjects": [],
            "m_AddedGameObjects": [], "m_AddedComponents": []}})
    .as_object()
    .unwrap()
    .clone()
}
pub(crate) fn name_modification(target: &str, guid: &str, name: &str) -> Value {
    json!({"target": {"fileID": target, "guid": guid, "type": 3}, "propertyPath": "m_Name",
        "value": name, "objectReference": reference("0")})
}
pub(crate) fn append_instance(
    asset: &mut TextAsset,
    file_id: &str,
    guid: &str,
    parent: &str,
    modifications: Vec<Value>,
) -> Result<()> {
    asset.append_document(
        1001,
        file_id,
        "PrefabInstance",
        &instance_record(guid, parent, modifications),
    )
}
pub(crate) fn alias_data(source_id: &str, guid: &str, instance: &str) -> Map<String, Value> {
    json!({"m_CorrespondingSourceObject": {"fileID": source_id, "guid": guid, "type": 3},
        "m_PrefabInstance": reference(instance), "m_PrefabAsset": reference("0")})
    .as_object()
    .unwrap()
    .clone()
}
/// Append a stripped alias record; `header` is the exact stripped header to store.
pub(crate) fn append_stripped(
    asset: &mut TextAsset,
    class_id: i64,
    file_id: &str,
    type_name: &str,
    data: &Map<String, Value>,
    header: String,
) -> Result<()> {
    asset.append_document(class_id, file_id, type_name, data)?;
    let position = asset.position(file_id).unwrap();
    asset.docs[position].header = header;
    asset.refresh()
}
impl Store {
    pub(crate) fn find_prefab_guid(&self, guid: &str) -> Result<String> {
        let mut found = String::new();
        for path in self.walk_assets()? {
            let kind = fs::symlink_metadata(&path)?.file_type();
            let relative = self.relative(&path);
            if kind.is_dir() || kind.is_symlink() || !relative.ends_with(".prefab.meta") {
                continue;
            }
            let text = String::from_utf8(read_limited(&path)?).unwrap();
            let matches: Vec<_> = guid_pattern().captures_iter(&text).collect();
            if matches.len() == 1 && matches[0][1].to_lowercase() == guid {
                if !found.is_empty() {
                    return Err(failure(
                        "duplicate_guid",
                        "prefab source GUID is ambiguous",
                        None,
                    ));
                }
                found = relative.strip_suffix(".meta").unwrap().to_string();
            }
        }
        if found.is_empty() {
            return Err(failure(
                "not_found",
                "authored prefab source GUID is not present under Assets",
                Some(json!({"guid": guid})),
            ));
        }
        Ok(found)
    }
    pub(crate) fn instantiate_prefab(
        &self,
        asset: &mut TextAsset,
        destination_path: &str,
        operation_id: &str,
        operation: &Operation,
    ) -> Result<BTreeMap<String, String>> {
        self.asset_path(&operation.prefab_path, &[".prefab"])?;
        let source = self.read_source(&operation.prefab_path)?;
        if operation.prefab_revision.is_empty() || source.revision != operation.prefab_revision {
            return Err(failure(
                "revision_conflict",
                "prefab source requires its exact current revision",
                Some(json!({"actualRevision": source.revision})),
            ));
        }
        let (graph, dependencies, _) = self.resolved_graph(&operation.prefab_path)?;
        check_graph_revisions(&dependencies, &operation.dependency_revisions)?;
        if destination_path.ends_with(".prefab")
            && dependencies
                .get(destination_path)
                .is_some_and(|revision| !revision.is_empty())
        {
            return Err(failure(
                "prefab_cycle",
                "instantiation would make this prefab depend on itself",
                None,
            ));
        }
        let mut root_transform: Option<&GraphObject> = None;
        for value in &graph.objects {
            if [4, 224].contains(&value.document.class_id)
                && id(value.document.data.get("m_Father").unwrap_or(&Value::Null)) == "0"
            {
                if root_transform.is_some() {
                    return Err(failure(
                        "invalid_prefab_root",
                        "prefab has multiple roots",
                        None,
                    ));
                }
                root_transform = Some(value);
            }
        }
        let root_transform = root_transform
            .ok_or_else(|| failure("invalid_prefab_root", "prefab has no root Transform", None))?;
        let owner = id(root_transform
            .document
            .data
            .get("m_GameObject")
            .unwrap_or(&Value::Null));
        let root_object = graph
            .objects
            .iter()
            .find(|value| value.document.class_id == 1 && value.document.file_id == owner)
            .ok_or_else(|| {
                failure(
                    "invalid_prefab_root",
                    "prefab root GameObject is absent",
                    None,
                )
            })?;
        let root_object_id = prefab_graph_identity(
            &root_object.document.file_id,
            &root_object.identities,
            &source.guid,
        )?;
        let root_transform_id = prefab_graph_identity(
            &root_transform.document.file_id,
            &root_transform.identities,
            &source.guid,
        )?;
        let parent_transform = if operation.parent_id == "0" {
            "0".to_string()
        } else {
            let (_, parent) = asset.plain_object(&operation.parent_id)?;
            asset.docs[parent].id().to_string()
        };
        let instance_id = asset.new_id(operation_id, &format!("{}.instance", operation.key));
        let object_id = asset.new_id(operation_id, &operation.key);
        let transform_id = asset.new_id(operation_id, &format!("{}.transform", operation.key));
        let mut modifications = Vec::new();
        if !operation.name.is_empty() {
            modifications.push(name_modification(
                &root_object_id,
                &source.guid,
                &operation.name,
            ));
        }
        append_instance(
            asset,
            &instance_id,
            &source.guid,
            &parent_transform,
            modifications,
        )?;
        for (file_id, source_id, type_name, class_id) in [
            (&object_id, &root_object_id, "GameObject", 1),
            (
                &transform_id,
                &root_transform_id,
                root_transform.document.type_name.as_str(),
                root_transform.document.class_id,
            ),
        ] {
            let header = asset.stripped_header(class_id, file_id);
            append_stripped(
                asset,
                class_id,
                file_id,
                type_name,
                &alias_data(source_id, &source.guid, &instance_id),
                header,
            )?;
        }
        asset.append_child(&parent_transform, &transform_id)?;
        Ok(BTreeMap::from([
            (String::new(), object_id),
            (".instance".to_string(), instance_id),
            (".transform".to_string(), transform_id),
        ]))
    }
    /// `request` is false when the value already holds typed numbers taken
    /// from authored YAML rather than JSON number text from a request.
    pub(crate) fn prefab_override(
        &self,
        asset: &mut TextAsset,
        operation: &Operation,
        request: bool,
    ) -> Result<()> {
        let instance = asset
            .find(&operation.instance_id)
            .filter(|doc| doc.class_id() == 1001 && !doc.stripped())
            .ok_or_else(|| {
                failure(
                    "not_found",
                    "instanceId must address an authored PrefabInstance",
                    None,
                )
            })?;
        if operation.target_file_id.is_empty()
            || operation.property.is_empty()
            || operation.property.ends_with(".Array.size")
        {
            return Err(failure(
                "invalid_request",
                "prefab override requires targetFileId and scalar/reference property; source array resizing is unsupported",
                None,
            ));
        }
        if operation
            .property
            .split('.')
            .any(|part| protected(part.split('[').next().unwrap_or_default()))
        {
            return Err(failure(
                "protected_property",
                "identity and ownership changes require a prefab graph operation",
                None,
            ));
        }
        let instance_id = instance.id().to_string();
        let source_guid = source_prefab_guid(instance.data());
        let modifications = items(&instance.field("m_Modification")["m_Modifications"]).to_vec();
        let path = self.find_prefab_guid(&source_guid)?;
        let (mut graph, revisions, _) = self.resolved_graph(&path)?;
        check_graph_revisions(&revisions, &operation.dependency_revisions)?;
        let target_guid = if operation.target_guid.is_empty() {
            source_guid.clone()
        } else {
            operation.target_guid.clone()
        };
        let target = graph
            .objects
            .iter_mut()
            .find(|object| {
                object.identities.iter().any(|identity| {
                    identity.guid == target_guid && identity.file_id == operation.target_file_id
                })
            })
            .ok_or_else(|| {
                failure(
                    "not_found",
                    "override target is outside the resolved prefab source graph",
                    None,
                )
            })?;
        let mut updated = modifications;
        let mut matched = None;
        for (index, item) in updated.iter().enumerate() {
            let target = &item["target"];
            let guid = match target["guid"].as_str() {
                Some(guid) if !guid.is_empty() => guid,
                _ => source_guid.as_str(),
            };
            if guid == target_guid
                && id(target) == operation.target_file_id
                && item["propertyPath"].as_str() == Some(operation.property.as_str())
            {
                if matched.is_some() {
                    return Err(failure(
                        "invalid_prefab",
                        "duplicate property override records require Editor normalization",
                        None,
                    ));
                }
                matched = Some(index);
            }
        }
        if operation.action == "revert_prefab_override" {
            let Some(index) = matched else {
                return Ok(());
            };
            updated.remove(index);
        } else {
            let convert = if request { normalize } else { normalize_typed };
            let mut value = convert(operation.value.as_ref().unwrap_or(&Value::Null))?;
            let mut reference_value = operation.object_reference.clone().map(Value::Object);
            if value.is_object() {
                reference_value = Some(std::mem::replace(&mut value, Value::String(String::new())));
            }
            let reference_value = convert(&reference_value.unwrap_or_else(|| reference("0")))?;
            if value.is_null() {
                value = Value::String(String::new());
            }
            if !(value.is_string() || value.is_i64() || value.is_f64()) {
                return Err(failure(
                    "invalid_property_value",
                    "prefab override must be a scalar or object reference",
                    None,
                ));
            }
            let text = sprint(&value);
            apply_override(
                &mut target.document.data,
                &operation.property,
                &Value::String(text.clone()),
                &reference_value,
            )
            .map_err(|error| {
                failure(
                    "unsupported_property",
                    error.to_string(),
                    Some(json!({"property": operation.property})),
                )
            })?;
            let mut item = json!({"target": {"fileID": operation.target_file_id, "guid": target_guid, "type": 3},
                "propertyPath": operation.property, "value": text, "objectReference": reference_value});
            match matched {
                None => updated.push(item),
                Some(index) => {
                    // Keep attributes this writer does not know about.
                    if let Some(old) = updated[index].as_object() {
                        let record = item.as_object_mut().unwrap();
                        for (key, value) in old {
                            if !record.contains_key(key) {
                                record.insert(key.clone(), value.clone());
                            }
                        }
                    }
                    updated[index] = item;
                }
            }
        }
        set_prefab_modification_field(
            asset,
            &instance_id,
            "m_Modifications",
            &Value::Array(updated),
        )
    }
}
