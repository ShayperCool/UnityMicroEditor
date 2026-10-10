use super::{
    Operation,
    hierarchy::id,
    prefab::source_prefab_guid,
    prefab_authoring::{PrefabEditRequest, PrefabPlanner, component_ids},
    prefab_linking::{prefab_graph_identity, prefab_roots, remap_prefab_data},
    prefab_serialization::set_prefab_modification_field,
    store::*,
    yaml_edit::{SEGMENT, TextAsset, external_reference, items, parse_property},
    yamlcodec::internal,
};
use crate::graph::{AuthoredGraph, GraphObject, apply_override};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
fn indexed(property: &str) -> String {
    property.replace(".Array.data[", "[")
}
pub(crate) fn modification_array(instance: &Map<String, Value>, key: &str) -> Vec<Value> {
    items(&instance.get("m_Modification").unwrap_or(&Value::Null)[key]).to_vec()
}
/// The resolved source object an override or addition record addresses.
pub(crate) fn source_override_target<'g>(
    graph: &'g AuthoredGraph,
    reference: &Value,
    default_guid: &str,
) -> Result<&'g GraphObject> {
    let guid = match reference["guid"].as_str() {
        Some(guid) if !guid.is_empty() => guid,
        _ => default_guid,
    };
    let file_id = id(reference);
    let mut target: Option<&GraphObject> = None;
    for object in &graph.objects {
        for identity in &object.identities {
            if identity.guid == guid && identity.file_id == file_id {
                if target.is_some_and(|found| found.document.file_id != object.document.file_id) {
                    return Err(failure(
                        "ambiguous_target",
                        "override target matches repeated nested prefab objects; use immediate source identity",
                        None,
                    ));
                }
                target = Some(object);
            }
        }
    }
    target.ok_or_else(|| {
        failure(
            "invalid_prefab_override",
            "override target is absent from resolved source",
            Some(json!({"guid": guid, "fileId": file_id})),
        )
    })
}
fn default_prefab_override(root: &str, transform: &str, target: &str, property: &str) -> bool {
    if target == root && property == "m_Name" {
        return true;
    }
    target == transform
        && [
            "m_LocalPosition",
            "m_LocalRotation",
            "m_LocalEulerAnglesHint",
            "m_RootOrder",
        ]
        .iter()
        .any(|prefix| {
            property == *prefix
                || property
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('.'))
        })
}
pub(crate) fn authored_property_value(data: &Map<String, Value>, path: &str) -> Result<Value> {
    let parts = parse_property(path)?;
    let mut current = Value::Object(data.clone());
    for part in parts {
        let capture = SEGMENT.captures(part).unwrap();
        let Value::Object(mut mapping) = current else {
            return Err(failure(
                "unsupported_property",
                "property traversal is not a mapping",
                None,
            ));
        };
        current = mapping.remove(&capture[1]).unwrap_or(Value::Null);
        if let Some(index) = capture.get(2) {
            let index = index
                .as_str()
                .parse::<usize>()
                .map_err(|_| internal("value out of range"))?;
            let Value::Array(mut items) = current else {
                return Err(failure(
                    "unsupported_property",
                    "property array element is absent",
                    None,
                ));
            };
            if index >= items.len() {
                return Err(failure(
                    "unsupported_property",
                    "property array element is absent",
                    None,
                ));
            }
            current = items.swap_remove(index);
        }
    }
    Ok(current)
}
impl PrefabPlanner<'_> {
    /// Positions in `graph.objects` of the addressed owner and the overridden object.
    fn override_target(&self, request: &PrefabEditRequest) -> Result<(usize, usize)> {
        let owner = self.target(&request.file_id, &request.target_path)?;
        if request.component_type.is_empty() {
            return Ok((owner, owner));
        }
        let mut target = None;
        for component in component_ids(&self.graph.objects[owner].document.data) {
            let Some(&index) = self.objects.get(&component) else {
                continue;
            };
            let value = &self.graph.objects[index].document;
            if value.type_name == request.component_type
                || format!("UnityEngine.{}", value.type_name) == request.component_type
                || value
                    .data
                    .get("m_EditorClassIdentifier")
                    .and_then(Value::as_str)
                    == Some(request.component_type.as_str())
            {
                if target.is_some() {
                    return Err(failure(
                        "ambiguous_target",
                        "multiple components match componentType",
                        None,
                    ));
                }
                target = Some(index);
            }
        }
        let target = target
            .ok_or_else(|| failure("not_found", "requested prefab component is absent", None))?;
        Ok((owner, target))
    }
    pub(crate) fn set_override(&mut self, request: &PrefabEditRequest) -> Result<()> {
        match &request.value {
            Some(Value::Object(mapping)) if !mapping.contains_key("fileID") => {
                if mapping.is_empty() {
                    return Err(failure(
                        "invalid_property_value",
                        "empty mapping override has no serialized leaves",
                        None,
                    ));
                }
                // serde_json maps iterate in sorted key order.
                for (key, value) in mapping {
                    let mut nested = request.clone();
                    nested.property = format!("{}.{key}", request.property);
                    nested.value = (!value.is_null()).then(|| value.clone());
                    self.set_override(&nested)?;
                }
                return Ok(());
            }
            Some(Value::Array(values)) => {
                let (_, target) = self.override_target(request)?;
                let old = authored_property_value(
                    &self.graph.objects[target].document.data,
                    &indexed(&request.property),
                )?;
                if old
                    .as_array()
                    .is_none_or(|items| items.len() != values.len())
                {
                    return Err(failure(
                        "unsupported_array_resize",
                        "array overrides must preserve existing serialized element count",
                        None,
                    ));
                }
                for (index, value) in values.iter().enumerate() {
                    let mut nested = request.clone();
                    nested.property = format!("{}.Array.data[{index}]", request.property);
                    nested.value = (!value.is_null()).then(|| value.clone());
                    self.set_override(&nested)?;
                }
                return self.write_scene();
            }
            _ => {}
        }
        let (owner, target) = self.override_target(request)?;
        let owner_id = self.graph.objects[owner].document.file_id.clone();
        let target_id = self.graph.objects[target].document.file_id.clone();
        let instance = self.instance(&owner_id)?;
        let guid = source_prefab_guid(self.asset.find(&instance).unwrap().data());
        let source_path = self.store.find_prefab_guid(&guid)?;
        let (graph, revisions) = self.include_graph(&source_path)?;
        let source_target = graph
            .objects
            .iter()
            .find(|value| format!("{instance}:{}", value.document.file_id) == target_id);
        let value = request.value.clone().unwrap_or(Value::Null);
        let Some(source_target) = source_target else {
            if self
                .asset
                .find(&target_id)
                .is_some_and(|doc| !doc.stripped() && ![1, 4, 224].contains(&doc.class_id()))
            {
                self.asset
                    .set_property(&target_id, &indexed(&request.property), &value)?;
                return self.write_scene();
            }
            return Err(failure(
                "unsupported_target",
                "added prefab objects require structural source overrides",
                None,
            ));
        };
        let source_id = prefab_graph_identity(
            &source_target.document.file_id,
            &source_target.identities,
            &guid,
        )?;
        self.store.prefab_override(
            &mut self.asset,
            &Operation {
                action: "set_prefab_override".into(),
                instance_id: instance,
                target_guid: guid,
                target_file_id: source_id,
                property: request.property.clone(),
                value: request.value.clone(),
                dependency_revisions: revisions,
                ..Operation::default()
            },
            true,
        )?;
        self.write_scene()
    }
    pub(crate) fn apply_revert(&mut self, request: &PrefabEditRequest) -> Result<()> {
        let owner = self.target(&request.file_id, &request.target_path)?;
        let owner_id = self.graph.objects[owner].document.file_id.clone();
        let instance = self.instance(&owner_id)?;
        let instance_data = self.asset.find(&instance).unwrap().data().clone();
        for key in [
            "m_RemovedComponents",
            "m_RemovedGameObjects",
            "m_AddedGameObjects",
        ] {
            if !modification_array(&instance_data, key).is_empty() {
                return Err(failure(
                    "unsupported_structural_override",
                    "this source prefab operation cannot apply/revert this structural override yet",
                    Some(json!({"field": key})),
                ));
            }
        }
        let source_guid = source_prefab_guid(&instance_data);
        let source_path = self.store.find_prefab_guid(&source_guid)?;
        let (graph, _) = self.include_graph(&source_path)?;
        let (root, transform) = prefab_roots(&graph)?;
        let (root, transform) = (
            root.document.file_id.clone(),
            transform.document.file_id.clone(),
        );
        let source = self.store.read_source(&source_path)?;
        let mut asset = TextAsset::parse(&source.content)?;
        let added = modification_array(&instance_data, "m_AddedComponents");
        if request.action == "apply" {
            self.apply_added_components(
                &mut asset,
                &graph,
                &instance,
                &added,
                &request.operation_id,
                true,
            )?;
        } else {
            let mut remove = HashSet::new();
            for entry in &added {
                let file_id = id(&entry["addedObject"]);
                if self.asset.find(&file_id).is_none_or(|doc| doc.stripped()) {
                    return Err(failure(
                        "invalid_prefab_override",
                        "added component is absent",
                        None,
                    ));
                }
                remove.insert(file_id);
            }
            self.asset.docs.retain(|doc| !remove.contains(doc.id()));
            self.asset.refresh()?;
        }
        let mut retained = Vec::new();
        for entry in modification_array(&instance_data, "m_Modifications") {
            let target = source_override_target(&graph, &entry["target"], &source_guid)?;
            let property = entry["propertyPath"].as_str().unwrap_or_default();
            if default_prefab_override(&root, &transform, &target.document.file_id, property) {
                retained.push(entry);
                continue;
            }
            if request.action == "apply" {
                self.apply_source_property(&mut asset, target, &entry)?;
            }
        }
        set_prefab_modification_field(
            &mut self.asset,
            &instance,
            "m_AddedComponents",
            &Value::Array(Vec::new()),
        )?;
        set_prefab_modification_field(
            &mut self.asset,
            &instance,
            "m_Modifications",
            &Value::Array(retained),
        )?;
        if request.action == "apply" {
            self.write_asset(&source_path, asset.text(), None)?;
        }
        self.write_scene()
    }
    /// Write one override record into the immediate source asset.
    pub(crate) fn apply_source_property(
        &mut self,
        asset: &mut TextAsset,
        target: &GraphObject,
        entry: &Value,
    ) -> Result<()> {
        let property = entry["propertyPath"].as_str().unwrap_or_default();
        let reference = &entry["objectReference"];
        if id(reference) != "0" && !external_reference(reference) {
            return Err(failure(
                "external_subtree_reference",
                "applying a scene object reference into a prefab requires an explicit source identity",
                None,
            ));
        }
        let target_id = &target.document.file_id;
        if asset
            .find(target_id)
            .is_some_and(|doc| !doc.stripped() && doc.class_id() != 1001)
        {
            let mut copied = remap_prefab_data(&target.document.data, &HashMap::new(), "", false)?;
            apply_override(&mut copied, property, &entry["value"], reference)
                .map_err(|error| failure("unsupported_property", error.to_string(), None))?;
            // Change only the addressed serialized span. Unity Array.data paths are
            // translated to the lossless source property's indexed notation.
            let normalized = indexed(property);
            let value = authored_property_value(&copied, &normalized)?;
            asset.set_property_typed(target_id, &normalized, &value)?;
            return Ok(());
        }
        let prefix = target_id.split(':').next().unwrap_or_default().to_string();
        let instance = asset
            .find(&prefix)
            .filter(|doc| doc.class_id() == 1001)
            .ok_or_else(|| {
                failure(
                    "unsupported_target",
                    "cannot locate inherited source property's authored instance",
                    None,
                )
            })?;
        let guid = source_prefab_guid(instance.data());
        // Remove exactly this enclosing instance's prefix before computing the
        // inherited identity in its immediate source prefab.
        let inner = target_id
            .strip_prefix(&format!("{prefix}:"))
            .unwrap_or(target_id);
        let source_id = prefab_graph_identity(inner, &target.identities, &guid)?;
        let source_path = self.store.find_prefab_guid(&guid)?;
        let (_, revisions) = self.include_graph(&source_path)?;
        let value = if id(reference) != "0" {
            reference.clone()
        } else {
            entry["value"].clone()
        };
        self.store.prefab_override(
            asset,
            &Operation {
                action: "set_prefab_override".into(),
                instance_id: prefix,
                target_guid: guid,
                target_file_id: source_id,
                property: property.into(),
                value: (!value.is_null()).then_some(value),
                object_reference: reference.as_object().cloned(),
                dependency_revisions: revisions,
                ..Operation::default()
            },
            false,
        )
    }
}
