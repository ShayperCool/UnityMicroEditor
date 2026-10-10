use super::{
    component_defaults::{apply_native_defaults, native_defaults_documents},
    hierarchy::id,
    prefab::{alias_data, append_stripped, source_prefab_guid},
    prefab_authoring::{PrefabEditRequest, PrefabPlanner},
    prefab_linking::{clone_prefab_document, owner_of, prefab_graph_identity, remap_prefab_data},
    prefab_overrides::{modification_array, source_override_target},
    prefab_serialization::set_prefab_modification_field,
    store::*,
    yaml_edit::{TextAsset, TextDocument, external_reference, reference, visit_map_references},
};
use crate::graph::{AuthoredGraph, GraphObject};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
fn graph_object<'g>(graph: &'g AuthoredGraph, file_id: &str) -> Option<&'g GraphObject> {
    graph
        .objects
        .iter()
        .find(|object| object.document.file_id == file_id)
}
fn added_component(source_owner: &str, guid: &str, component: &str) -> Value {
    json!({"targetCorrespondingSourceObject": {"fileID": source_owner, "guid": guid, "type": 3},
        "insertIndex": -1, "addedObject": reference(component)})
}
/// Create the local stripped identity that Unity uses when a newly authored
/// component references an inherited owner or companion.
pub(crate) fn ensure_prefab_alias(
    asset: &mut TextAsset,
    graph: &AuthoredGraph,
    object: &GraphObject,
    operation: &str,
) -> Result<String> {
    let (file_id, class_id) = (&object.document.file_id, object.document.class_id);
    if asset
        .find(file_id)
        .is_some_and(|doc| doc.class_id() == class_id)
    {
        return Ok(file_id.clone());
    }
    if let Some(doc) = asset
        .docs
        .iter()
        .find(|doc| doc.class_id() == class_id && graph.local_ids.get(doc.id()) == Some(file_id))
    {
        return Ok(doc.id().to_string());
    }
    let prefix = file_id.split(':').next().unwrap_or_default().to_string();
    let instance = asset
        .find(&prefix)
        .filter(|doc| doc.class_id() == 1001)
        .ok_or_else(|| {
            failure(
                "invalid_identity",
                "inherited object has no authored instance",
                None,
            )
        })?;
    let guid = source_prefab_guid(instance.data());
    let inner = file_id
        .strip_prefix(&format!("{prefix}:"))
        .unwrap_or(file_id);
    let source_id = prefab_graph_identity(inner, &object.identities, &guid)?;
    if let Some(doc) = asset.docs.iter().find(|doc| {
        let corresponding = doc.field("m_CorrespondingSourceObject");
        doc.class_id() == class_id
            && doc.stripped()
            && id(doc.field("m_PrefabInstance")) == prefix
            && corresponding["guid"].as_str() == Some(guid.as_str())
            && id(corresponding) == source_id
    }) {
        return Ok(doc.id().to_string());
    }
    let alias = asset.new_id(operation, &format!("prefab-alias/{file_id}"));
    let mut data = alias_data(&source_id, &guid, &prefix);
    if class_id != 1 {
        let owner = graph_object(graph, &owner_of(object))
            .ok_or_else(|| failure("invalid_graph", "inherited component owner is absent", None))?;
        let owner = ensure_prefab_alias(asset, graph, owner, operation)?;
        data.insert("m_GameObject".into(), reference(&owner));
    }
    let header = asset.stripped_header(class_id, &alias);
    append_stripped(
        asset,
        class_id,
        &alias,
        &object.document.type_name,
        &data,
        header,
    )?;
    Ok(alias)
}
impl PrefabPlanner<'_> {
    pub(crate) fn add_component(&mut self, request: &PrefabEditRequest) -> Result<()> {
        let template = request.defaults_template.as_ref().ok_or_else(|| {
            failure(
                "component_defaults_missing",
                "prefab components require a publicly captured version-bound default template",
                None,
            )
        })?;
        let owner = self.target(&request.file_id, &request.target_path)?;
        let owner_id = self.graph.objects[owner].document.file_id.clone();
        if self
            .asset
            .find(&owner_id)
            .is_some_and(|doc| !doc.stripped() && id(doc.field("m_PrefabInstance")) == "0")
        {
            self.created = apply_native_defaults(
                &mut self.asset,
                &request.operation_id,
                &owner_id,
                "",
                &request.component_type,
                &request.fields,
                template,
            )?;
            return self.write_scene();
        }
        let instance = self.instance(&owner_id)?;
        let transform = self
            .transform(&owner_id)
            .ok_or_else(|| failure("invalid_graph", "prefab transform is absent", None))?;
        let (defaults, _, components) =
            native_defaults_documents(template, &request.component_type)?;
        if self.graph.objects[transform].document.class_id != 224
            && components
                .iter()
                .any(|&position| defaults.docs[position].class_id() == 224)
        {
            return Err(failure(
                "unsupported_transform_replacement",
                "an inherited Transform cannot become RectTransform while its prefab link remains; unpack first",
                None,
            ));
        }
        let selected: HashSet<String> = self
            .graph
            .objects
            .iter()
            .map(|object| object.document.file_id.clone())
            .collect();
        let mapping = self.stable_local_ids(&selected, &request.operation_id);
        let mut temp = TextAsset::empty(&self.asset.prefix, self.asset.newline);
        for object in &self.graph.objects {
            let mut data =
                remap_prefab_data(&object.document.data, &mapping, &self.source.guid, false)?;
            for key in [
                "m_CorrespondingSourceObject",
                "m_PrefabInstance",
                "m_PrefabAsset",
            ] {
                if data.contains_key(key) {
                    data.insert(key.into(), reference("0"));
                }
            }
            temp.append_document(
                object.document.class_id,
                &mapping[&object.document.file_id],
                &object.document.type_name,
                &data,
            )?;
        }
        let before: HashSet<String> = temp.docs.iter().map(|doc| doc.id().to_string()).collect();
        let created = apply_native_defaults(
            &mut temp,
            &request.operation_id,
            &mapping[&owner_id],
            "",
            &request.component_type,
            &request.fields,
            template,
        )?;
        let guid = source_prefab_guid(self.asset.find(&instance).unwrap().data());
        let inner = owner_id
            .strip_prefix(&format!("{instance}:"))
            .unwrap_or(&owner_id);
        let source_owner =
            prefab_graph_identity(inner, &self.graph.objects[owner].identities, &guid)?;
        let owner_alias = ensure_prefab_alias(
            &mut self.asset,
            &self.graph,
            &self.graph.objects[owner],
            &request.operation_id,
        )?;
        let mut local = HashMap::from([(mapping[&owner_id].clone(), owner_alias)]);
        let added: Vec<&TextDocument> = temp
            .docs
            .iter()
            .filter(|doc| !before.contains(doc.id()))
            .collect();
        for doc in &added {
            if self.asset.contains(doc.id()) {
                return Err(failure(
                    "invalid_identity",
                    "new component collides with an authored identity",
                    None,
                ));
            }
            local.insert(doc.id().to_string(), doc.id().to_string());
        }
        // Capture only companions actually referenced by the new components.
        for doc in &added {
            let mut targets = Vec::new();
            visit_map_references(doc.data(), &mut |reference| {
                let file_id = id(reference);
                if file_id != "0" && !external_reference(reference) {
                    targets.push(file_id);
                }
                Ok(())
            })?;
            for file_id in targets {
                if local.get(&file_id).is_some_and(|alias| !alias.is_empty()) {
                    continue;
                }
                let expanded = mapping
                    .iter()
                    .find(|(_, value)| **value == file_id)
                    .map(|(expanded, _)| expanded.clone())
                    .ok_or_else(|| {
                        failure(
                            "invalid_identity",
                            "default component references an unknown scene identity",
                            Some(json!({"fileId": file_id})),
                        )
                    })?;
                let object = &self.graph.objects[self.objects[&expanded]];
                let alias = ensure_prefab_alias(
                    &mut self.asset,
                    &self.graph,
                    object,
                    &request.operation_id,
                )?;
                local.insert(file_id, alias);
            }
        }
        // Alias creation refreshes documents; read the current instance before
        // editing its existing modification map.
        let mut entries = modification_array(
            self.asset.find(&instance).unwrap().data(),
            "m_AddedComponents",
        );
        for doc in &added {
            let copy = clone_prefab_document(doc, &local, "", true)?;
            entries.push(added_component(&source_owner, &guid, copy.id()));
            self.asset.docs.push(copy);
        }
        self.asset.refresh()?;
        set_prefab_modification_field(
            &mut self.asset,
            &instance,
            "m_AddedComponents",
            &Value::Array(entries),
        )?;
        self.created = created;
        self.write_scene()
    }
    /// Copy exact authored component serialization into the immediate prefab,
    /// retaining scene aliases when its link replaces the override.
    pub(crate) fn apply_added_components(
        &mut self,
        asset: &mut TextAsset,
        graph: &AuthoredGraph,
        instance: &str,
        entries: &[Value],
        operation: &str,
        retain_scene_aliases: bool,
    ) -> Result<HashMap<String, String>> {
        let guid = source_prefab_guid(
            self.asset
                .find(instance)
                .map(|doc| doc.data())
                .ok_or_else(|| failure("not_found", "prefab instance is absent", None))?,
        );
        let mut ids: HashMap<String, String> = HashMap::new();
        let mut owners: HashMap<String, &GraphObject> = HashMap::new();
        for entry in entries {
            let owner =
                source_override_target(graph, &entry["targetCorrespondingSourceObject"], &guid)?;
            if owner.document.class_id != 1 {
                return Err(failure(
                    "invalid_prefab_override",
                    "added component target must be a GameObject",
                    None,
                ));
            }
            let file_id = id(&entry["addedObject"]);
            if self
                .asset
                .find(&file_id)
                .is_none_or(|doc| doc.stripped() || [1, 4, 224, 1001].contains(&doc.class_id()))
            {
                return Err(failure(
                    "invalid_prefab_override",
                    "added component source is absent or invalid",
                    None,
                ));
            }
            if ids.contains_key(&file_id) {
                return Err(failure(
                    "invalid_prefab_override",
                    "duplicate added component record",
                    None,
                ));
            }
            let target = if asset.contains(&file_id) {
                asset.new_id(operation, &format!("applied-component/{file_id}"))
            } else {
                file_id.clone()
            };
            ids.insert(file_id.clone(), target);
            owners.insert(file_id, owner);
        }
        for entry in entries {
            let old_id = id(&entry["addedObject"]);
            let owner = owners[&old_id];
            let alias = ensure_prefab_alias(asset, graph, owner, operation)?;
            let doc = self.asset.find(&old_id).unwrap().clone();
            let scene_owner = id(doc.field("m_GameObject"));
            let mut mapping = ids.clone();
            mapping.insert(scene_owner.clone(), alias.clone());
            let mut targets = Vec::new();
            visit_map_references(doc.data(), &mut |reference| {
                let file_id = id(reference);
                if file_id != "0" && !external_reference(reference) {
                    targets.push(file_id);
                }
                Ok(())
            })?;
            for file_id in targets {
                if mapping.get(&file_id).is_some_and(|value| !value.is_empty()) {
                    continue;
                }
                let expanded = self
                    .graph
                    .local_ids
                    .get(&file_id)
                    .filter(|expanded| !expanded.is_empty())
                    .unwrap_or(&file_id);
                let Some(inner) = expanded.strip_prefix(&format!("{instance}:")) else {
                    return Err(failure(
                        "external_subtree_reference",
                        "applying a component with outside scene references into a prefab is unsupported",
                        Some(json!({"fileId": file_id})),
                    ));
                };
                let target = graph_object(graph, inner).ok_or_else(|| {
                    failure(
                        "invalid_identity",
                        "component reference lacks proven prefab source identity",
                        None,
                    )
                })?;
                let alias = ensure_prefab_alias(asset, graph, target, operation)?;
                mapping.insert(file_id, alias);
            }
            let copy = clone_prefab_document(&doc, &mapping, &self.source.guid, true)?;
            let copy_id = copy.id().to_string();
            asset.docs.push(copy);
            asset.refresh()?;
            let owner_position = asset.position(&alias).unwrap();
            if !asset.docs[owner_position].stripped() {
                let mut components = asset.component_ids(owner_position);
                components.push(copy_id.clone());
                asset.set_components(&alias, &components)?;
            } else {
                let owner_id = &owner.document.file_id;
                let inner_instance = owner_id.split(':').next().unwrap_or_default().to_string();
                let instance_data = asset
                    .find(&inner_instance)
                    .ok_or_else(|| {
                        failure(
                            "invalid_identity",
                            "inherited object has no authored instance",
                            None,
                        )
                    })?
                    .data()
                    .clone();
                let inner_guid = source_prefab_guid(&instance_data);
                let inner_owner = owner_id
                    .strip_prefix(&format!("{inner_instance}:"))
                    .unwrap_or(owner_id);
                let source_owner =
                    prefab_graph_identity(inner_owner, &owner.identities, &inner_guid)?;
                let mut values = modification_array(&instance_data, "m_AddedComponents");
                values.push(added_component(&source_owner, &inner_guid, &copy_id));
                set_prefab_modification_field(
                    asset,
                    &inner_instance,
                    "m_AddedComponents",
                    &Value::Array(values),
                )?;
            }
            if retain_scene_aliases {
                let mut data = alias_data(&copy_id, &guid, instance);
                data.insert("m_GameObject".into(), reference(&scene_owner));
                let mut strip = TextAsset::empty(&self.asset.prefix, self.asset.newline);
                strip.append_document(doc.class_id(), doc.id(), &doc.document.type_name, &data)?;
                let mut alias = strip.docs.remove(0);
                alias.header = self.asset.stripped_header(doc.class_id(), doc.id());
                alias.document.stripped = true;
                if let Some(position) = self.asset.position(doc.id()) {
                    self.asset.docs[position] = alias;
                }
            }
        }
        if retain_scene_aliases {
            self.asset.refresh()?;
        }
        Ok(ids)
    }
}
