use super::{
    hierarchy::id,
    store::*,
    yaml_edit::{TextAsset, visit_map_references},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
/// Operation addresses local Unity fileIDs as strings, never JSON numbers.
/// Creation keys allow subsequent operations in the same transaction to use
/// "$key" and "$key.transform" before IDs have been published.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Operation {
    pub action: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub property: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub script_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub component_type: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub class_id: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template_file_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub template_revision: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefab_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefab_revision: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instance_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_guid: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_file_id: String,
    #[serde(default, skip_serializing_if = "no_entries")]
    pub object_reference: Option<Map<String, Value>>,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub dependency_revisions: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rect_transform: bool,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub fields: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recursive: bool,
}
fn is_zero(value: &i64) -> bool {
    *value == 0
}
/// Omits both an absent and an empty map, yet they remain distinct values.
fn no_entries(value: &Option<Map<String, Value>>) -> bool {
    value.as_ref().is_none_or(Map::is_empty)
}
/// A field the document sets to null is left at its zero value.
pub(crate) fn nullable<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditRequest {
    pub path: String,
    pub expected_revision: String,
    pub operation_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub operations: Vec<Operation>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preview: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct EditResult {
    #[serde(flatten)]
    pub mutation: MutationResult,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub preview: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content: String,
}
fn change(file_id: &str, action: &str) -> Change {
    Change {
        file_id: file_id.into(),
        action: action.into(),
        ..Change::default()
    }
}
impl Store {
    fn edit_plan(&self, request: &EditRequest) -> Result<MutationPlan> {
        let source = self.read_source(&request.path)?;
        if source.revision != request.expected_revision {
            return Err(failure(
                "revision_conflict",
                "authored asset or metadata changed",
                Some(
                    json!({"actualRevision": source.revision, "expectedRevision": request.expected_revision}),
                ),
            ));
        }
        let mut asset = TextAsset::parse(&source.content)?;
        let original = TextAsset::parse(&source.content)?;
        let mut created = BTreeMap::<String, String>::new();
        let mut allowed_prefab_changes = HashSet::new();
        let mut changes = Vec::new();
        for (index, requested) in request.operations.iter().enumerate() {
            let mut operation = requested.clone();
            let resolve = |file_id: &str| match file_id.strip_prefix('$') {
                Some(key) => created.get(key).cloned().unwrap_or_default(),
                None => file_id.to_string(),
            };
            operation.file_id = resolve(&operation.file_id);
            operation.instance_id = resolve(&operation.instance_id);
            if operation.parent_id.starts_with('$') {
                operation.parent_id = resolve(&operation.parent_id);
                if operation.parent_id.is_empty() {
                    return Err(failure(
                        "not_found",
                        "creation key for parent does not exist",
                        None,
                    ));
                }
            }
            if operation.parent_id.is_empty() {
                operation.parent_id = "0".into();
            }
            let action = operation.action.clone();
            let creation_key = |operation: &mut Operation| {
                if operation.key.is_empty() {
                    operation.key = index.to_string();
                }
                if created.contains_key(&operation.key) {
                    return Err(failure("invalid_request", "duplicate creation key", None));
                }
                Ok(())
            };
            match action.as_str() {
                "instantiate_prefab" => {
                    creation_key(&mut operation)?;
                    let ids = self.instantiate_prefab(
                        &mut asset,
                        &request.path,
                        &request.operation_id,
                        &operation,
                    )?;
                    let mut item = change(&ids[".instance"], &action);
                    item.after = Some(json!(operation.prefab_path));
                    for (suffix, file_id) in ids {
                        created.insert(format!("{}{suffix}", operation.key), file_id);
                    }
                    changes.push(item);
                }
                "set_prefab_override" | "revert_prefab_override" => {
                    self.prefab_override(&mut asset, &operation, true)?;
                    allowed_prefab_changes.insert(operation.instance_id.clone());
                    let mut item = change(&operation.instance_id, &action);
                    item.property = operation.property;
                    item.after = operation.value;
                    changes.push(item);
                }
                "set_property" => {
                    let before = asset.set_property(
                        &operation.file_id,
                        &operation.property,
                        operation.value.as_ref().unwrap_or(&Value::Null),
                    )?;
                    let mut item = change(&operation.file_id, &action);
                    item.property = operation.property;
                    item.before = (!before.is_null()).then_some(before);
                    item.after = operation.value;
                    changes.push(item);
                }
                "create_gameobject" => {
                    creation_key(&mut operation)?;
                    if operation.name.is_empty() || operation.name.len() > 1024 {
                        return Err(failure(
                            "invalid_request",
                            "GameObject name must contain 1 to 1024 bytes",
                            None,
                        ));
                    }
                    let (object, transform) = if operation.rect_transform {
                        let fields = self.rect_transform_fields(&operation)?;
                        asset.create_object_with_transform(
                            &request.operation_id,
                            &operation.key,
                            &operation.name,
                            &operation.parent_id,
                            224,
                            &fields,
                        )?
                    } else {
                        asset.create_object(
                            &request.operation_id,
                            &operation.key,
                            &operation.name,
                            &operation.parent_id,
                        )?
                    };
                    created.insert(operation.key.clone(), object.clone());
                    created.insert(format!("{}.transform", operation.key), transform);
                    let mut item = change(&object, &action);
                    item.after = Some(json!(operation.name));
                    changes.push(item);
                }
                "reparent" => {
                    let before = asset.reparent(&operation.file_id, &operation.parent_id)?;
                    let mut item = change(&operation.file_id, &action);
                    item.before = Some(json!(before));
                    item.after = Some(json!(operation.parent_id));
                    changes.push(item);
                }
                "delete_gameobject" => {
                    let removed = asset.delete_object(&operation.file_id, operation.recursive)?;
                    let mut item = change(&operation.file_id, &action);
                    item.before = Some(json!(removed));
                    changes.push(item);
                }
                "add_component" => {
                    creation_key(&mut operation)?;
                    if !operation.component_type.is_empty()
                        || operation.class_id != 0
                        || !operation.template_path.is_empty()
                    {
                        let component = self.add_native_component(
                            &mut asset,
                            &request.operation_id,
                            &operation,
                        )?;
                        created.insert(operation.key.clone(), component.clone());
                        let mut item = change(&component, &action);
                        item.after = Some(json!(operation.component_type));
                        changes.push(item);
                        continue;
                    }
                    self.asset_path(&operation.script_path, &[".cs"])?;
                    let script = self.read_source(&operation.script_path)?;
                    let component = asset.add_component(
                        &request.operation_id,
                        &operation.key,
                        &operation.file_id,
                        &script.guid,
                        &operation.fields,
                    )?;
                    created.insert(operation.key.clone(), component.clone());
                    let mut item = change(&component, &action);
                    item.after = Some(json!(operation.script_path));
                    changes.push(item);
                }
                "remove_component" => {
                    asset.remove_component(&operation.file_id)?;
                    changes.push(change(&operation.file_id, &action));
                }
                _ => {
                    return Err(failure(
                        "unsupported_operation",
                        "unknown source edit action",
                        Some(json!({"action": action})),
                    ));
                }
            }
        }
        asset.validate(&source.guid)?;
        asset.preserve_prefab_documents_except(&original, &allowed_prefab_changes)?;
        if request.path.ends_with(".prefab") {
            asset.validate_prefab_root()?;
        }
        let removed: HashSet<String> = original
            .docs
            .iter()
            .filter(|doc| !asset.contains(doc.id()))
            .map(|doc| doc.id().to_string())
            .collect();
        if !removed.is_empty() {
            self.check_external_references(&source, &removed)?;
        }
        Ok(MutationPlan {
            metadata: source.metadata.as_bytes().to_vec(),
            source,
            content: asset.text().into_bytes(),
            created,
            changes,
        })
    }
    pub fn edit(&self, request: &EditRequest) -> Result<EditResult> {
        self.asset_path(&request.path, &[".unity", ".prefab", ".asset", ".mat"])?;
        if request.operations.is_empty() || request.operations.len() > 256 {
            return Err(failure(
                "invalid_request",
                "provide 1 to 256 edit operations",
                None,
            ));
        }
        if request.preview {
            return self.locked(|| {
                if request.expected_revision.is_empty() || request.operation_id.is_empty() {
                    return Err(failure(
                        "invalid_request",
                        "preview requires expectedRevision and operationId",
                        None,
                    ));
                }
                let plan = self.edit_plan(request)?;
                Ok(EditResult {
                    mutation: MutationResult {
                        operation_id: request.operation_id.clone(),
                        path: request.path.clone(),
                        guid: plan.source.guid,
                        previous_revision: plan.source.revision,
                        revision: revision(&plan.content, &plan.metadata),
                        changed: plan.source.content.as_bytes() != plan.content,
                        replayed: false,
                        created_ids: plan.created,
                        changes: plan.changes,
                    },
                    preview: true,
                    content: String::from_utf8(plan.content)
                        .map_err(|error| failure("internal_error", error.to_string(), None))?,
                })
            });
        }
        let mutation = self.mutate(
            &request.path,
            &request.expected_revision,
            &request.operation_id,
            request,
            || self.edit_plan(request),
        )?;
        Ok(EditResult {
            mutation,
            preview: false,
            content: String::new(),
        })
    }
    pub(crate) fn check_external_references(
        &self,
        source: &Source,
        removed: &HashSet<String>,
    ) -> Result<()> {
        self.check_external_references_overlay(source, removed, &BTreeMap::new())
    }
    pub(crate) fn check_external_references_overlay(
        &self,
        source: &Source,
        removed: &HashSet<String>,
        overlay: &BTreeMap<String, Source>,
    ) -> Result<()> {
        let info = self.inspect_unlocked()?;
        let mut paths: Vec<String> = info.scenes;
        paths.extend(info.prefabs);
        // Include arbitrary text materials and ScriptableObjects, not just scene graphs.
        for path in self.walk_assets()? {
            let kind = std::fs::symlink_metadata(&path)?.file_type();
            if kind.is_dir() || kind.is_symlink() {
                continue;
            }
            let relative = self.relative(&path);
            if matches!(
                path_ext(&relative).to_lowercase().as_str(),
                ".mat" | ".asset"
            ) {
                paths.push(relative);
            }
        }
        paths.extend(
            overlay
                .keys()
                .filter(|path| path_ext(path) != ".cs")
                .cloned(),
        );
        let mut seen = HashSet::new();
        for path in paths {
            if path == source.path || !seen.insert(path.clone()) {
                continue;
            }
            let other = match overlay.get(&path) {
                Some(other) => other.clone(),
                None => self.read_source(&path)?,
            };
            if !other.content.contains(&source.guid) {
                continue;
            }
            let asset = TextAsset::parse(&other.content)?;
            for doc in &asset.docs {
                visit_map_references(doc.data(), &mut |reference| {
                    let target = id(reference);
                    if reference["guid"].as_str() == Some(source.guid.as_str())
                        && removed.contains(&target)
                    {
                        return Err(failure(
                            "referenced_object",
                            "deleted object is referenced by another authored asset",
                            Some(
                                json!({"path": path, "sourceFileId": doc.id(), "targetFileId": target}),
                            ),
                        ));
                    }
                    Ok(())
                })?;
            }
        }
        Ok(())
    }
}
