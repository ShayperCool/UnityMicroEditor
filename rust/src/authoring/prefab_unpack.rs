use super::{
    hierarchy::id,
    prefab::{alias_data, append_stripped, source_prefab_guid},
    prefab_authoring::{PrefabEditRequest, PrefabPlanner},
    prefab_linking::{
        clone_prefab_document, owner_of, prefab_graph_identity, prefab_roots, remap_prefab_data,
    },
    prefab_overrides::{modification_array, source_override_target},
    prefab_serialization::set_prefab_modification_field,
    store::*,
    yaml_edit::{TextAsset, external_reference, reference, visit_map_references},
};
use crate::graph::GraphObject;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
fn father(object: &GraphObject) -> String {
    id(object.document.data.get("m_Father").unwrap_or(&Value::Null))
}
impl PrefabPlanner<'_> {
    /// Existing serialized aliases are retained so surviving scene references
    /// keep their identities. Newly materialized objects get deterministic IDs.
    pub(crate) fn stable_local_ids(
        &self,
        selected: &HashSet<String>,
        operation: &str,
    ) -> HashMap<String, String> {
        let mut result: HashMap<String, String> = HashMap::new();
        for doc in &self.asset.docs {
            if let Some(expanded) = self.graph.local_ids.get(doc.id())
                && selected.contains(expanded)
                && !result.contains_key(expanded)
            {
                result.insert(expanded.clone(), doc.id().to_string());
            }
        }
        let mut keys: Vec<&String> = selected.iter().collect();
        keys.sort();
        let mut used: HashSet<String> = result.values().cloned().collect();
        for key in keys {
            if result.contains_key(key) {
                continue;
            }
            for attempt in 0.. {
                let candidate = self
                    .asset
                    .new_id(operation, &format!("unpack/{key}/{attempt}"));
                if used.insert(candidate.clone()) {
                    result.insert(key.clone(), candidate);
                    break;
                }
            }
        }
        result
    }
    pub(crate) fn unpack(&mut self, request: &PrefabEditRequest) -> Result<()> {
        let owner = self.target(&request.file_id, &request.target_path)?;
        let owner_id = self.graph.objects[owner].document.file_id.clone();
        let instance = self.instance(&owner_id)?;
        let prefix = format!("{instance}:");
        let mut root = None;
        for value in &self.graph.objects {
            if [4, 224].contains(&value.document.class_id)
                && value.document.file_id.starts_with(&prefix)
                && !father(value).starts_with(&prefix)
            {
                if root.is_some() {
                    return Err(failure(
                        "invalid_prefab_root",
                        "unpack instance has multiple root transforms",
                        None,
                    ));
                }
                root = Some(self.objects[&value.document.file_id]);
            }
        }
        let root = root.ok_or_else(|| {
            failure(
                "invalid_prefab_root",
                "unpack instance root is absent",
                None,
            )
        })?;
        let root_id = self.graph.objects[root].document.file_id.clone();
        let root_owner = owner_of(&self.graph.objects[root]);
        let selected = self.selected(&root_owner)?;
        let mapping = self.stable_local_ids(&selected, &request.operation_id);
        let mapped = |key: &str| mapping.get(key).cloned().unwrap_or_default();
        let (replacement, root_reference) = if request.completely {
            let mut replacement = TextAsset::empty(&self.asset.prefix, self.asset.newline);
            for value in &self.graph.objects {
                if !selected.contains(&value.document.file_id) {
                    continue;
                }
                let mut data =
                    remap_prefab_data(&value.document.data, &mapping, &self.source.guid, false)?;
                for key in [
                    "m_CorrespondingSourceObject",
                    "m_PrefabInstance",
                    "m_PrefabAsset",
                ] {
                    if data.contains_key(key) {
                        data.insert(key.into(), reference("0"));
                    }
                }
                replacement.append_document(
                    value.document.class_id,
                    &mapped(&value.document.file_id),
                    &value.document.type_name,
                    &data,
                )?;
            }
            (replacement, mapped(&root_id))
        } else {
            self.partial_unpack(request, &instance, root, &mapping)?
        };
        // Rewrite surviving references to any aliases collapsed to one materialized
        // object, and replace native SceneRoots instance entries explicitly.
        let mut local_remap = HashMap::from([(instance.clone(), root_reference)]);
        for (local, expanded) in &self.graph.local_ids {
            if let Some(target) = mapping.get(expanded).filter(|target| !target.is_empty()) {
                local_remap.insert(local.clone(), target.clone());
            }
        }
        let mut kept = TextAsset::empty(&self.asset.prefix, self.asset.newline);
        for doc in &self.asset.docs {
            if doc.id() == instance
                || self
                    .graph
                    .local_ids
                    .get(doc.id())
                    .is_some_and(|expanded| selected.contains(expanded))
                || id(doc.field("m_PrefabInstance")) == instance
            {
                continue;
            }
            // Preserve every unrelated document exactly unless one of its references
            // actually needs changing as part of this source graph transaction.
            let mut changed = false;
            visit_map_references(doc.data(), &mut |reference| {
                let file_id = id(reference);
                if local_remap
                    .get(&file_id)
                    .is_some_and(|target| !target.is_empty() && *target != file_id)
                    && !external_reference(reference)
                {
                    changed = true;
                }
                Ok(())
            })?;
            kept.docs.push(if changed {
                clone_prefab_document(doc, &local_remap, &self.source.guid, false)?
            } else {
                doc.clone()
            });
        }
        kept.docs.extend(replacement.docs);
        kept.refresh()?;
        self.created.insert("unpacked".into(), mapped(&root_owner));
        self.created
            .insert("unpacked.transform".into(), mapped(&root_id));
        let path = self.source.path.clone();
        self.write_asset(&path, kept.text(), None)
    }
    /// Unpack only the outermost link: the immediate source prefab's documents
    /// with this instance's overrides and additions applied.
    fn partial_unpack(
        &mut self,
        request: &PrefabEditRequest,
        instance: &str,
        scene_root: usize,
        mapping: &HashMap<String, String>,
    ) -> Result<(TextAsset, String)> {
        let instance_data = self.asset.find(instance).unwrap().data().clone();
        for key in [
            "m_RemovedComponents",
            "m_RemovedGameObjects",
            "m_AddedGameObjects",
        ] {
            if !modification_array(&instance_data, key).is_empty() {
                return Err(failure(
                    "unsupported_structural_override",
                    "outermost unpack of structural overrides is not yet supported",
                    Some(json!({"field": key})),
                ));
            }
        }
        let guid = source_prefab_guid(&instance_data);
        let path = self.store.find_prefab_guid(&guid)?;
        let source = self.store.read_source(&path)?;
        let (graph, _) = self.include_graph(&path)?;
        let mut template = TextAsset::parse(&source.content)?;
        for entry in modification_array(&instance_data, "m_Modifications") {
            let target = source_override_target(&graph, &entry["target"], &guid)?;
            self.apply_source_property(&mut template, target, &entry)?;
        }
        let added_mapping = self.apply_added_components(
            &mut template,
            &graph,
            instance,
            &modification_array(&instance_data, "m_AddedComponents"),
            &request.operation_id,
            false,
        )?;
        let (source_root, source_transform) = prefab_roots(&graph)?;
        let mapped = |key: &str| mapping.get(key).cloned().unwrap_or_default();
        let mut local_mapping: HashMap<String, String> = HashMap::new();
        for (local, expanded) in &graph.local_ids {
            let target = mapped(&format!("{instance}:{expanded}"));
            if !target.is_empty() {
                local_mapping.insert(local.clone(), target);
            }
        }
        for (scene_id, source_id) in &added_mapping {
            local_mapping.insert(source_id.clone(), mapped(scene_id));
        }
        for doc in &template.docs {
            if !doc.stripped()
                || local_mapping
                    .get(doc.id())
                    .is_some_and(|target| !target.is_empty())
            {
                continue;
            }
            let corresponding = doc.field("m_CorrespondingSourceObject");
            let reference_guid = corresponding["guid"].as_str().unwrap_or_default();
            let prefix = format!("{}:", id(doc.field("m_PrefabInstance")));
            let mut target = String::new();
            for object in &graph.objects {
                let Some(inner) = object.document.file_id.strip_prefix(&prefix) else {
                    continue;
                };
                if prefab_graph_identity(inner, &object.identities, reference_guid)
                    .is_ok_and(|identity| identity == id(corresponding))
                {
                    target = mapped(&format!("{instance}:{}", object.document.file_id));
                    break;
                }
            }
            if target.is_empty() {
                return Err(failure(
                    "invalid_identity",
                    "partial unpack could not prove an added companion alias",
                    None,
                ));
            }
            local_mapping.insert(doc.id().to_string(), target);
        }
        for doc in &template.docs {
            if doc.class_id() == 1001 {
                local_mapping.insert(
                    doc.id().to_string(),
                    self.asset.new_id(
                        &request.operation_id,
                        &format!("retained-instance/{}", doc.id()),
                    ),
                );
            }
        }
        let mut output = TextAsset::empty(&self.asset.prefix, self.asset.newline);
        for doc in &template.docs {
            output
                .docs
                .push(clone_prefab_document(doc, &local_mapping, &guid, false)?);
        }
        output.refresh()?;
        let owner_id = mapped(&format!("{instance}:{}", source_root.document.file_id));
        let transform_id = mapped(&format!("{instance}:{}", source_transform.document.file_id));
        let mut parent = father(&self.graph.objects[scene_root]);
        if let Some(candidate) = mapping
            .get(&parent)
            .filter(|candidate| !candidate.is_empty())
        {
            parent = candidate.clone();
        }
        if output
            .find(&transform_id)
            .is_some_and(|doc| !doc.stripped())
        {
            output.set_top_level(&transform_id, "m_Father", &reference(&parent))?;
            return Ok((output, transform_id));
        }
        // Unpacking the outer instance of a variant retains its immediate base
        // instance. Add explicit aliases for the surviving scene root identities.
        let prefix = source_transform
            .document
            .file_id
            .split(':')
            .next()
            .unwrap_or_default();
        let base_instance = local_mapping
            .get(prefix)
            .filter(|base| output.find(base).is_some_and(|doc| doc.class_id() == 1001))
            .cloned()
            .ok_or_else(|| {
                failure(
                    "unsupported_variant",
                    "partial unpack cannot identify variant's immediate base instance",
                    None,
                )
            })?;
        set_prefab_modification_field(
            &mut output,
            &base_instance,
            "m_TransformParent",
            &reference(&parent),
        )?;
        let base_guid = source_prefab_guid(output.find(&base_instance).unwrap().data());
        for (object, file_id) in [(source_root, &owner_id), (source_transform, &transform_id)] {
            if output.contains(file_id) {
                continue;
            }
            let inner = object
                .document
                .file_id
                .strip_prefix(&format!("{prefix}:"))
                .unwrap_or(&object.document.file_id);
            let source_id = prefab_graph_identity(inner, &object.identities, &base_guid)?;
            let header = format!("--- !u!{} &{file_id} stripped\n", object.document.class_id);
            append_stripped(
                &mut output,
                object.document.class_id,
                file_id,
                &object.document.type_name,
                &alias_data(&source_id, &base_guid, &base_instance),
                header,
            )?;
        }
        Ok((output, base_instance))
    }
}
