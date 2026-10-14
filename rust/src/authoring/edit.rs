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
                    let value = operation.value.as_ref().unwrap_or(&Value::Null);
                    // A field the component's script declares and the file
                    // does not hold yet (one added since the file was saved,
                    // also by a hot reload patch) is written as a new key.
                    let before =
                        match asset.set_property(&operation.file_id, &operation.property, value) {
                            Err(error)
                                if error.code == "unsupported_property"
                                    && error.message
                                        == "property path is absent in authored data" =>
                            {
                                self.add_declared_field(
                                    &mut asset,
                                    &operation.file_id,
                                    &operation.property,
                                    value,
                                )
                                .map_err(|refusal| refusal.unwrap_or(error))?;
                                Value::Null
                            }
                            other => other?,
                        };
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
    /// Writes `property` into a MonoBehaviour that lacks it, when it is a
    /// top-level field that the component's script declares as one Unity
    /// serializes: public or `[SerializeField]`, not static, const or
    /// readonly. `Err(None)` when that cannot be shown, so the absent path
    /// stands; `Err(Some(...))` for a script that declares no such field.
    fn add_declared_field(
        &self,
        asset: &mut TextAsset,
        file_id: &str,
        property: &str,
        value: &Value,
    ) -> std::result::Result<(), Option<AuthoringError>> {
        let Some(document) = asset.find(file_id) else {
            return Err(None);
        };
        if document.class_id() != 114 || !super::script::identifier(property) {
            return Err(None);
        }
        let guid = document.field("m_Script")["guid"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase();
        let script = self.find_script_guid(&guid).map_err(|_| None)?;
        let read = |path: &str| {
            read_limited(&self.root.join(path))
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
        };
        let source = read(&script).ok_or(None)?;
        // The script's class is named like its file; an inherited field is
        // declared by a base class in a file of its own (session6-7).
        let mut class = script
            .rsplit('/')
            .next()
            .and_then(|file| file.strip_suffix(".cs"))
            .unwrap_or_default()
            .to_string();
        let mut text = source;
        let mut declared = None;
        for _ in 0..8 {
            let (field, base) = declared_field(&text, &class, property);
            if field.is_some() {
                declared = field;
                break;
            }
            let Some(next) = base.and_then(|base| {
                let path = self.find_class_source(&base)?;
                Some((base, read(&path)?))
            }) else {
                break;
            };
            (class, text) = next;
        }
        let Some(field) = declared.filter(|field| field.serialized) else {
            return Err(Some(failure(
                "unsupported_property",
                "property path is absent in authored data, and the component's script declares no field of that name that Unity serializes",
                Some(json!({"property": property, "script": script})),
            )));
        };
        // A value Unity would read as the field's default is refused, as on
        // a key the data holds (session6-6).
        if let Err(expected) = fits_declared_type(&field.type_text, value) {
            return Err(Some(failure(
                "invalid_property_value",
                &format!(
                    "the field {property} is declared as {} and takes {expected}",
                    field.type_text
                ),
                Some(json!({"property": property, "script": script, "type": field.type_text})),
            )));
        }
        asset
            .insert_top_level(
                file_id,
                property,
                &super::yaml_edit::normalize(value).map_err(Some)?,
            )
            .map_err(Some)
    }

    /// The script source under Assets that declares `class <name>`, when
    /// one file does.
    fn find_class_source(&self, name: &str) -> Option<String> {
        let pattern =
            regex::Regex::new(&format!(r"\b(class|struct)\s+{}\b", regex::escape(name))).ok()?;
        let mut found = Vec::new();
        for path in self.walk_assets().ok()? {
            let relative = self.relative(&path);
            if !relative.ends_with(".cs") {
                continue;
            }
            let Some(text) = read_limited(&path)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
            else {
                continue;
            };
            if pattern.is_match(&code_only(&text)) {
                found.push(relative);
            }
        }
        (found.len() == 1).then(|| found.remove(0))
    }

    /// The script source under Assets whose `.meta` holds `guid`.
    fn find_script_guid(&self, guid: &str) -> Result<String> {
        let mut found = Vec::new();
        for path in self.walk_assets()? {
            let relative = self.relative(&path);
            if !relative.ends_with(".cs.meta")
                || std::fs::symlink_metadata(&path)?.file_type().is_symlink()
            {
                continue;
            }
            let text = String::from_utf8(read_limited(&path)?).unwrap_or_default();
            let matches: Vec<_> = guid_pattern().captures_iter(&text).collect();
            if matches.len() == 1 && matches[0][1].to_lowercase() == guid {
                found.push(relative.strip_suffix(".meta").unwrap().to_string());
            }
        }
        match found.as_slice() {
            [only] => Ok(only.clone()),
            _ => Err(failure(
                "not_found",
                "the component's script is not one source under Assets",
                Some(json!({"guid": guid})),
            )),
        }
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

/// A field a class declares, as its source has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclaredField {
    /// The type as written (`int`, `List<string>`, `Vector3`, `Stats[]`).
    pub type_text: String,
    /// Unity serializes it: public or `[SerializeField]`, not static, const,
    /// readonly or `[NonSerialized]`, and not of a delegate type.
    pub serialized: bool,
}

/// The source with its comments and the text of its string and character
/// literals blanked, at the same byte offsets.
fn code_only(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for byte in &mut out[from..to] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let end = source[i..].find('\n').map_or(bytes.len(), |at| i + at);
                blank(&mut out, i, end);
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let end = source[i + 2..].find("*/").map_or(bytes.len(), |at| i + 2 + at + 2);
                blank(&mut out, i, end);
                i = end;
            }
            quote @ (b'"' | b'\'') => {
                let verbatim = i > 0 && bytes[i - 1] == b'@';
                let mut j = i + 1;
                while j < bytes.len() {
                    if bytes[j] == b'\\' && !verbatim {
                        j += 2;
                        continue;
                    }
                    if bytes[j] == quote {
                        if verbatim && bytes.get(j + 1) == Some(&quote) {
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    j += 1;
                }
                let end = (j + 1).min(bytes.len());
                blank(&mut out, i + 1, end.saturating_sub(1).max(i + 1));
                i = end;
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).unwrap_or_default()
}

/// The byte after the bracket that closes the one at `open`.
fn closing(code: &[u8], open: usize) -> usize {
    let (left, right) = (code[open], if code[open] == b'{' { b'}' } else { b')' });
    let mut depth = 0usize;
    for (offset, byte) in code[open..].iter().enumerate() {
        if *byte == left {
            depth += 1;
        } else if *byte == right {
            depth -= 1;
            if depth == 0 {
                return open + offset + 1;
            }
        }
    }
    code.len()
}

/// The body of `class <name>` in the code and the first type of its base
/// list, when it has one.
fn class_body<'a>(code: &'a str, name: &str) -> Option<(&'a str, Option<String>)> {
    let pattern = regex::Regex::new(&format!(r"\b(class|struct)\s+{}\b", regex::escape(name))).ok()?;
    let found = pattern.find(code)?;
    let open = found.end() + code[found.end()..].find('{')?;
    let head = &code[found.end()..open];
    let base = head
        .split_once(':')
        .map(|(_, list)| list.split(" where ").next().unwrap_or(list))
        .and_then(|list| {
            let mut depth = 0i32;
            let mut first = String::new();
            for character in list.chars() {
                match character {
                    '<' => depth += 1,
                    '>' => depth -= 1,
                    ',' if depth == 0 => break,
                    _ => {}
                }
                first.push(character);
            }
            let first = first.trim();
            let bare = first.split('<').next().unwrap_or(first).trim();
            let bare = bare.rsplit('.').next().unwrap_or(bare);
            (!bare.is_empty()).then(|| bare.to_string())
        });
    let end = closing(code.as_bytes(), open);
    Some((&code[open + 1..end.saturating_sub(1)], base))
}

/// The fields a class body declares, by name. A member with a body (a
/// method, a property, a nested type) is skipped, and so is an
/// expression-bodied one; a field initializer may hold braces.
fn body_fields(body: &str, delegates: &[String]) -> Vec<(String, DeclaredField)> {
    let code = body.as_bytes();
    let mut fields = Vec::new();
    let mut statement = String::new();
    let mut i = 0;
    while i < code.len() {
        match code[i] {
            b'{' if !(statement.contains('=') && !statement.contains("=>")) => {
                // A member's body ends the member.
                i = closing(code, i);
                statement.clear();
                continue;
            }
            b'{' | b'(' => {
                let end = closing(code, i);
                statement.push_str(&body[i..end]);
                i = end;
                continue;
            }
            b';' => {
                fields.extend(statement_fields(&statement, delegates));
                statement.clear();
            }
            byte => statement.push(byte as char),
        }
        i += 1;
    }
    fields
}

/// The fields one declaration statement declares (`public int ammo, clips`).
fn statement_fields(statement: &str, delegates: &[String]) -> Vec<(String, DeclaredField)> {
    let mut text = statement.trim();
    if text.starts_with('#') {
        text = text.split_once('\n').map_or("", |(_, rest)| rest).trim();
    }
    // Preprocessor lines inside the statement are no part of it.
    let text: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let mut text = text.trim();
    let mut attributes = Vec::new();
    while text.starts_with('[') {
        let Some(end) = text.find(']') else {
            return Vec::new();
        };
        attributes.push(text[1..end].to_string());
        text = text[end + 1..].trim_start();
    }
    let assigned = text.find('=');
    if text.contains("=>") && assigned.is_none_or(|at| text[at..].starts_with("=>")) {
        return Vec::new();
    }
    let head_end = assigned.unwrap_or(text.len());
    if text[..head_end].contains('(') {
        return Vec::new();
    }
    // Declarators split at commas outside brackets; initializers dropped.
    let mut parts = Vec::new();
    let (mut depth, mut current) = (0i32, String::new());
    for character in text.chars() {
        match character {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    parts.push(current);
    let head = parts[0].split('=').next().unwrap_or("").trim().to_string();
    let words: Vec<&str> = head.split_whitespace().collect();
    let Some((name, before)) = words.split_last() else {
        return Vec::new();
    };
    let modifiers = [
        "public", "private", "protected", "internal", "static", "readonly", "const", "volatile",
        "new", "unsafe", "fixed", "event", "using",
    ];
    let has = |word: &str| before.contains(&word);
    let type_text = before
        .iter()
        .filter(|word| !modifiers.contains(word))
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    if type_text.is_empty() || has("event") || has("using") || has("const") && type_text.is_empty() {
        return Vec::new();
    }
    let attribute = |wanted: &str| {
        attributes.iter().any(|group| {
            group.split(',').any(|item| {
                let item = item.trim();
                let item = item.split('(').next().unwrap_or(item).trim();
                let item = item.rsplit('.').next().unwrap_or(item);
                item == wanted || item == format!("{wanted}Attribute")
            })
        })
    };
    let bare = type_text.split('<').next().unwrap_or(&type_text).trim();
    let bare = bare.rsplit('.').next().unwrap_or(bare);
    let delegate = matches!(bare, "Action" | "Func" | "Predicate" | "EventHandler" | "Comparison" | "UnityAction")
        || delegates.iter().any(|known| known == bare);
    let serialized = !has("static")
        && !has("const")
        && !has("readonly")
        && !attribute("NonSerialized")
        && !delegate
        && (has("public") || attribute("SerializeField"));
    let mut names = vec![name.to_string()];
    for part in &parts[1..] {
        if let Some(other) = part.split('=').next().map(str::trim).filter(|other| super::script::identifier(other)) {
            names.push(other.to_string());
        }
    }
    names
        .into_iter()
        .filter(|name| super::script::identifier(name))
        .map(|name| {
            (
                name,
                DeclaredField {
                    type_text: type_text.clone(),
                    serialized,
                },
            )
        })
        .collect()
}

/// The field `name` of the class `class` as the C# source declares it, and
/// the base class to look in when it does not.
pub(crate) fn declared_field(source: &str, class: &str, name: &str) -> (Option<DeclaredField>, Option<String>) {
    let code = code_only(source);
    let delegates: Vec<String> = regex::Regex::new(r"\bdelegate\s+[^;(]*?\b([A-Za-z_][A-Za-z0-9_]*)\s*[<(]")
        .unwrap()
        .captures_iter(&code)
        .map(|capture| capture[1].to_string())
        .collect();
    let Some((body, base)) = class_body(&code, class) else {
        return (None, None);
    };
    let field = body_fields(body, &delegates)
        .into_iter()
        .find(|(declared, _)| declared == name)
        .map(|(_, field)| field);
    (field, base)
}

/// Whether C# source declares `name` as a field Unity serializes in its
/// first class (the script's): see [`declared_field`].
#[cfg(test)]
pub(crate) fn declares_serialized_field(source: &str, name: &str) -> bool {
    let code = code_only(source);
    let Some(class) = regex::Regex::new(r"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)")
        .unwrap()
        .captures(&code)
        .map(|capture| capture[1].to_string())
    else {
        return false;
    };
    declared_field(source, &class, name)
        .0
        .is_some_and(|field| field.serialized)
}

/// Whether a JSON value fits a field of the C# type: a number for a number
/// or an enum, a boolean or 0/1 for bool, a string for string, the
/// components of a vector, quaternion or color, a sequence of fitting values
/// for an array or a List, and a mapping or a number (an enum) for a type the
/// source alone cannot tell (an object reference, a [Serializable] class).
pub(crate) fn fits_declared_type(type_text: &str, value: &Value) -> std::result::Result<(), String> {
    let text = type_text.trim().trim_start_matches("global::");
    let text = text
        .strip_prefix("System.")
        .or_else(|| text.strip_prefix("UnityEngine."))
        .unwrap_or(text);
    if let Some(element) = text.strip_suffix("[]") {
        return match value {
            Value::Array(items) => items
                .iter()
                .try_for_each(|item| fits_declared_type(element, item)),
            _ => Err(format!("a sequence of {element}")),
        };
    }
    if let Some(element) = text
        .strip_prefix("List<")
        .or_else(|| text.strip_prefix("Collections.Generic.List<"))
        .and_then(|rest| rest.strip_suffix('>'))
    {
        return match value {
            Value::Array(items) => items
                .iter()
                .try_for_each(|item| fits_declared_type(element, item)),
            _ => Err(format!("a sequence of {element}")),
        };
    }
    let number = |value: &Value| matches!(value, Value::Number(_));
    let components = |keys: &[&str]| match value {
        Value::Object(map)
            if map.len() == keys.len()
                && keys.iter().all(|key| map.get(*key).is_some_and(number)) =>
        {
            Ok(())
        }
        _ => Err(format!("a mapping of {}", keys.join(", "))),
    };
    match text {
        "int" | "uint" | "long" | "ulong" | "short" | "ushort" | "byte" | "sbyte" | "float"
        | "double" | "char" | "Int32" | "UInt32" | "Int64" | "UInt64" | "Int16" | "UInt16"
        | "Byte" | "SByte" | "Single" | "Double" => {
            number(value).then_some(()).ok_or_else(|| "a number".into())
        }
        "bool" | "Boolean" => match value {
            Value::Bool(_) => Ok(()),
            Value::Number(n) if n.as_i64().is_some_and(|n| n == 0 || n == 1) => Ok(()),
            _ => Err("true, false, 0 or 1".into()),
        },
        "string" | "String" => matches!(value, Value::String(_))
            .then_some(())
            .ok_or_else(|| "a string".into()),
        "Vector2" | "Vector2Int" => components(&["x", "y"]),
        "Vector3" | "Vector3Int" => components(&["x", "y", "z"]),
        "Vector4" | "Quaternion" => components(&["x", "y", "z", "w"]),
        "Color" | "Color32" => components(&["r", "g", "b", "a"]),
        "Rect" | "RectInt" => components(&["x", "y", "width", "height"]),
        _ => match value {
            Value::Object(_) | Value::Number(_) => Ok(()),
            _ => Err("a reference ({\"fileID\": ...}), the fields of the object, or the number of an enum value".into()),
        },
    }
}
