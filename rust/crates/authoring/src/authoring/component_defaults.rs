use super::{
    components::native_component_types,
    edit::nullable,
    hierarchy::{hierarchy, id, sprint},
    store::*,
    yaml_edit::{TextAsset, external_reference, json_round_trip, visit_references_mut},
};
use crate::{ENGINE_VERSION, sha256};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
/// A version/profile-bound snapshot obtained once through Unity's public
/// component creation API. The caller verifies its profile against the installed
/// engine and resolved packages before source publication.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NativeDefaultsTemplate {
    pub schema_version: i64,
    pub profile: String,
    pub unity_version: String,
    pub unity_revision: String,
    pub component_type: String,
    pub content: String,
    pub sha256: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeDefaultsRequest {
    pub path: String,
    pub expected_revision: String,
    pub operation_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_path: String,
    pub component_type: String,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub fields: BTreeMap<String, Value>,
    #[serde(default)]
    pub template: NativeDefaultsTemplate,
}
fn invalid(message: &str) -> AuthoringError {
    failure("invalid_component_defaults", message, None)
}
/// The parsed template, its owner GameObject and the owner's components.
pub(crate) fn native_defaults_documents(
    template: &NativeDefaultsTemplate,
    requested: &str,
) -> Result<(TextAsset, usize, Vec<usize>)> {
    let name = requested.strip_prefix("UnityEngine.").unwrap_or(requested);
    if template.schema_version != 1
        || template.profile.is_empty()
        || template.unity_version != ENGINE_VERSION
        || template.unity_revision.is_empty()
        || template.sha256 != sha256(template.content.as_bytes())
        || template
            .component_type
            .strip_prefix("UnityEngine.")
            .unwrap_or(&template.component_type)
            != name
        || template.content.len() > MAX_SOURCE
    {
        return Err(invalid(
            "cached component defaults lack their exact version/profile/content identity",
        ));
    }
    let asset = TextAsset::parse(&template.content)?;
    let mut owner = None;
    for (index, doc) in asset.docs.iter().enumerate() {
        if doc.class_id() == 1 {
            if owner.is_some() || doc.stripped() || id(doc.field("m_PrefabInstance")) != "0" {
                return Err(invalid(
                    "defaults must contain one ordinary template GameObject",
                ));
            }
            owner = Some(index);
        }
    }
    let owner = owner.ok_or_else(|| invalid("template GameObject is absent"))?;
    let types = native_component_types();
    let mut components = Vec::new();
    let mut seen = HashSet::new();
    let (mut transforms, mut selected) = (0, 0);
    for component_id in asset.component_ids(owner) {
        let position = asset.position(&component_id).filter(|&position| {
            let doc = &asset.docs[position];
            !seen.contains(&component_id)
                && !doc.stripped()
                && id(doc.field("m_GameObject")) == asset.docs[owner].id()
                && id(doc.field("m_PrefabInstance")) == "0"
        });
        let position =
            position.ok_or_else(|| invalid("template component ownership is invalid"))?;
        seen.insert(component_id);
        let doc = &asset.docs[position];
        if [4, 224].contains(&doc.class_id()) {
            transforms += 1;
        } else if types.get(&doc.class_id()).copied().unwrap_or_default() != doc.document.type_name
        {
            return Err(invalid(
                "template contains an unsupported companion component",
            ));
        }
        if doc.document.type_name == name {
            selected += 1;
        }
        components.push(position);
    }
    if transforms != 1 || selected != 1 || components.len() > 32 {
        return Err(invalid(
            "template requires one transform and one selected native component",
        ));
    }
    asset.validate("")?;
    Ok((asset, owner, components))
}
fn defaults_target(asset: &TextAsset, file_id: &str, target_path: &str) -> Result<String> {
    if !file_id.is_empty() {
        if !target_path.is_empty() {
            return Err(failure(
                "invalid_request",
                "select a component owner by fileId or targetPath",
                None,
            ));
        }
        return Ok(file_id.into());
    }
    if target_path.is_empty() {
        return Err(failure(
            "invalid_request",
            "component owner requires fileId or targetPath",
            None,
        ));
    }
    let docs: Vec<_> = asset.docs.iter().map(|doc| doc.document.clone()).collect();
    let view = hierarchy(&Source::default(), &docs, "");
    let objects: HashMap<&str, &super::hierarchy::GameObject> = view
        .objects
        .iter()
        .map(|object| (object.file_id.as_str(), object))
        .collect();
    let wanted = target_path.strip_prefix('/').unwrap_or(target_path);
    let mut found = String::new();
    for object in &view.objects {
        let mut parts = vec![object.name.as_str()];
        let mut seen = HashSet::from([object.file_id.as_str()]);
        let mut parent = object.parent_id.as_str();
        while parent != "0" {
            let value = objects
                .get(parent)
                .filter(|_| !seen.contains(parent))
                .ok_or_else(|| {
                    failure(
                        "invalid_graph",
                        "component target hierarchy has an absent or cyclic parent",
                        None,
                    )
                })?;
            seen.insert(parent);
            parts.insert(0, value.name.as_str());
            parent = value.parent_id.as_str();
        }
        if parts.join("/") == wanted {
            if !found.is_empty() {
                return Err(failure(
                    "ambiguous_target",
                    "component target path matches multiple GameObjects",
                    None,
                ));
            }
            found = object.file_id.clone();
        }
    }
    if found.is_empty() {
        return Err(failure(
            "not_found",
            "component target path is absent",
            None,
        ));
    }
    Ok(found)
}
fn defaults_data(
    input: &Map<String, Value>,
    remap: &HashMap<String, String>,
) -> Result<Map<String, Value>> {
    let mut output = json_round_trip(&Value::Object(input.clone()))?;
    visit_references_mut(&mut output, &mut |reference| {
        let value = Value::Object(reference.clone());
        if external_reference(&value) || id(&value) == "0" {
            return Ok(());
        }
        let target = remap.get(&id(&value)).ok_or_else(|| {
            failure(
                "invalid_component_defaults",
                "template local reference leaves the captured component closure",
                Some(json!({"fileId": sprint(&reference["fileID"])})),
            )
        })?;
        reference.insert("fileID".into(), Value::String(target.clone()));
        Ok(())
    })?;
    match output {
        Value::Object(map) => Ok(map),
        _ => unreachable!(),
    }
}
/// Shared with the source prefab planner. The caller must already hold the
/// source transaction and supply an ordinary materialized owner.
pub(crate) fn apply_native_defaults(
    asset: &mut TextAsset,
    operation_id: &str,
    file_id: &str,
    target_path: &str,
    component_type: &str,
    fields: &BTreeMap<String, Value>,
    template: &NativeDefaultsTemplate,
) -> Result<BTreeMap<String, String>> {
    let (source, template_owner, components) = native_defaults_documents(template, component_type)?;
    let owner_id = defaults_target(asset, file_id, target_path)?;
    let (owner, transform) = asset.plain_object(&owner_id)?;
    let transform_id = asset.docs[transform].id().to_string();
    let transform_class = asset.docs[transform].class_id();
    let transform_data = asset.docs[transform].data().clone();
    let requested = component_type
        .strip_prefix("UnityEngine.")
        .unwrap_or(component_type);
    let mut owner_components = asset.component_ids(owner);
    let mut existing: HashMap<i64, String> = HashMap::new();
    for component_id in &owner_components {
        let doc = asset
            .find(component_id)
            .ok_or_else(|| failure("invalid_graph", "target component is absent", None))?;
        if doc.document.type_name == requested {
            return Err(failure(
                "component_exists",
                "the requested native component already exists on the target",
                None,
            ));
        }
        existing
            .entry(doc.class_id())
            .or_insert_with(|| component_id.clone());
    }
    let mut remap = HashMap::from([(
        source.docs[template_owner].id().to_string(),
        owner_id.clone(),
    )]);
    let mut created = BTreeMap::new();
    for &position in &components {
        let component = &source.docs[position];
        if [4, 224].contains(&component.class_id()) {
            remap.insert(component.id().into(), transform_id.clone());
            continue;
        }
        let target = match existing.get(&component.class_id()) {
            Some(companion) => companion.clone(),
            None => asset.new_id(operation_id, &format!("native-default.{}", component.id())),
        };
        remap.insert(component.id().into(), target.clone());
        created.insert(
            format!("UnityEngine.{}", component.document.type_name),
            target,
        );
    }
    let selected = created
        .get(&format!("UnityEngine.{requested}"))
        .cloned()
        .unwrap_or_default();
    created.insert(component_type.into(), selected.clone());
    for &position in &components {
        let component = &source.docs[position];
        if [4, 224].contains(&component.class_id()) {
            if component.class_id() == 224 && transform_class == 4 {
                let mut data = defaults_data(component.data(), &remap)?;
                for (field, value) in &transform_data {
                    if ![
                        "serializedVersion",
                        "m_GameObject",
                        "m_CorrespondingSourceObject",
                        "m_PrefabInstance",
                        "m_PrefabAsset",
                    ]
                    .contains(&field.as_str())
                    {
                        data.insert(field.clone(), value.clone());
                    }
                }
                // Replace the Transform in place so document order and identity survive.
                let index = asset.position(&transform_id).unwrap();
                asset.docs.remove(index);
                asset.append_document(224, &transform_id, "RectTransform", &data)?;
                let added = asset.docs.pop().unwrap();
                asset.docs.insert(index, added);
                asset.refresh()?;
                created.insert("UnityEngine.RectTransform".into(), transform_id.clone());
            }
            continue;
        }
        if existing.contains_key(&component.class_id()) {
            continue;
        }
        let data = defaults_data(component.data(), &remap)?;
        let target = remap[component.id()].clone();
        asset.append_document(
            component.class_id(),
            &target,
            &component.document.type_name,
            &data,
        )?;
        owner_components.push(target);
    }
    asset.set_components(&owner_id, &owner_components)?;
    // BTreeMap iteration is sorted key order.
    for (key, value) in fields {
        if key == "m_ObjectHideFlags" {
            return Err(failure(
                "protected_property",
                "component fields cannot replace identity or ownership",
                None,
            ));
        }
        asset.set_property(&selected, key, value)?;
    }
    Ok(created)
}
impl Store {
    /// Publishes the component and all required companions in one existing
    /// source journal. No Editor process or authored scratch asset is used.
    /// `before_publish` binds external native-default context immediately before writing.
    pub fn add_native_defaults(
        &self,
        request: &NativeDefaultsRequest,
        before_publish: Option<&dyn Fn() -> Result<()>>,
    ) -> Result<MutationResult> {
        self.asset_path(&request.path, &[".unity", ".prefab"])?;
        self.mutate_with_guard(
            &request.path,
            &request.expected_revision,
            &request.operation_id,
            request,
            before_publish,
            || {
                let source = self.read_source(&request.path)?;
                if source.revision != request.expected_revision {
                    return Err(failure(
                        "revision_conflict",
                        "authored source changed before cached component publication",
                        Some(json!({"actualRevision": source.revision})),
                    ));
                }
                let mut asset = TextAsset::parse(&source.content)?;
                let original = TextAsset::parse(&source.content)?;
                let created = apply_native_defaults(
                    &mut asset,
                    &request.operation_id,
                    &request.file_id,
                    &request.target_path,
                    &request.component_type,
                    &request.fields,
                    &request.template,
                )?;
                asset.validate(&source.guid)?;
                asset.preserve_prefab_documents(&original)?;
                if request.path.ends_with(".prefab") {
                    asset.validate_prefab_root()?;
                }
                let component = created
                    .get(&request.component_type)
                    .cloned()
                    .unwrap_or_default();
                Ok(MutationPlan {
                    metadata: source.metadata.as_bytes().to_vec(),
                    source,
                    content: asset.text().into_bytes(),
                    created,
                    changes: vec![Change {
                        file_id: component,
                        action: "add_component".into(),
                        after: Some(json!(request.component_type)),
                        ..Change::default()
                    }],
                })
            },
        )
    }
}
