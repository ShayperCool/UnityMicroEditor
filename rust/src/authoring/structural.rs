use super::{
    hierarchy::id,
    store::*,
    yaml_edit::{TextAsset, ids_from_list, items, normalize, protected, reference, reference_list},
};
use crate::sha256;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
pub(crate) fn base_fields() -> Map<String, Value> {
    json!({"m_ObjectHideFlags":0,"m_CorrespondingSourceObject":{"fileID":"0"},"m_PrefabInstance":{"fileID":"0"},"m_PrefabAsset":{"fileID":"0"}})
        .as_object()
        .unwrap()
        .clone()
}
fn component_entries(ids: &[String]) -> Value {
    Value::Array(
        ids.iter()
            .map(|id| json!({"component": reference(id)}))
            .collect(),
    )
}
impl TextAsset {
    /// Positions of an ordinary GameObject and its authored Transform.
    pub(crate) fn plain_object(&self, file_id: &str) -> Result<(usize, usize)> {
        let object = self
            .position(file_id)
            .filter(|&index| self.docs[index].class_id() == 1)
            .ok_or_else(|| {
                failure(
                    "not_found",
                    "GameObject fileID is absent",
                    Some(json!({"fileId": file_id})),
                )
            })?;
        let doc = &self.docs[object];
        if doc.stripped() || id(doc.field("m_PrefabInstance")) != "0" {
            return Err(failure(
                "unsupported_prefab_write",
                "prefab instance and stripped GameObjects require override-aware authoring",
                None,
            ));
        }
        for (index, doc) in self.docs.iter().enumerate() {
            if [4, 224].contains(&doc.class_id()) && id(doc.field("m_GameObject")) == file_id {
                if doc.stripped() || id(doc.field("m_PrefabInstance")) != "0" {
                    return Err(failure(
                        "unsupported_prefab_write",
                        "prefab instance transforms cannot be changed structurally",
                        None,
                    ));
                }
                return Ok((object, index));
            }
        }
        Err(failure(
            "invalid_graph",
            "GameObject requires an authored Transform",
            None,
        ))
    }
    pub(crate) fn new_id(&self, operation: &str, key: &str) -> String {
        for attempt in 0u64.. {
            let digest = sha256(format!("{operation}\0{key}\0{attempt}").as_bytes());
            let value = u64::from_str_radix(&digest[..16], 16).unwrap() & ((1 << 56) - 1);
            if value != 0 {
                let id = value.to_string();
                if !self.contains(&id) {
                    return id;
                }
            }
        }
        unreachable!()
    }
    fn update_child(&mut self, parent_transform: &str, child: &str, append: bool) -> Result<()> {
        let (owner, key) = if parent_transform == "0" {
            match self.root_document() {
                Some(roots) => (roots.id().to_string(), "m_Roots"),
                None => return Ok(()),
            }
        } else {
            if !self.contains(parent_transform) {
                return Err(failure("invalid_graph", "parent transform absent", None));
            }
            (parent_transform.to_string(), "m_Children")
        };
        let mut ids = ids_from_list(self.find(&owner).unwrap().field(key));
        if append {
            ids.push(child.into());
        } else {
            ids.retain(|id| id != child);
        }
        self.set_top_level(&owner, key, &reference_list(&ids))
    }
    pub(crate) fn append_child(&mut self, parent_transform: &str, child: &str) -> Result<()> {
        self.update_child(parent_transform, child, true)
    }
    pub(crate) fn detach_child(&mut self, parent_transform: &str, child: &str) -> Result<()> {
        self.update_child(parent_transform, child, false)
    }
    pub(crate) fn require_resolved_subtree(&self, transform: &str) -> Result<()> {
        let mut seen = HashSet::new();
        let mut pending = vec![transform.to_string()];
        while let Some(file_id) = pending.pop() {
            if !seen.insert(file_id.clone()) {
                return Err(failure(
                    "transform_cycle",
                    "cycle in authored Transform subtree",
                    None,
                ));
            }
            let doc = self.find(&file_id).ok_or_else(|| {
                failure(
                    "invalid_graph",
                    "Transform subtree references an absent object",
                    None,
                )
            })?;
            if doc.stripped() || id(doc.field("m_PrefabInstance")) != "0" {
                return Err(failure(
                    "unsupported_prefab_write",
                    "structural changes to a subtree containing unresolved prefab instances are unsupported",
                    Some(json!({"fileId": file_id})),
                ));
            }
            // Reversed so the explicit stack visits children in authored order.
            pending.extend(ids_from_list(doc.field("m_Children")).into_iter().rev());
        }
        Ok(())
    }
    pub(crate) fn create_object(
        &mut self,
        operation: &str,
        key: &str,
        name: &str,
        parent: &str,
    ) -> Result<(String, String)> {
        self.create_object_with_transform(operation, key, name, parent, 4, &BTreeMap::new())
    }
    pub(crate) fn create_object_with_transform(
        &mut self,
        operation: &str,
        key: &str,
        name: &str,
        parent: &str,
        class_id: i64,
        fields: &BTreeMap<String, Value>,
    ) -> Result<(String, String)> {
        let parent_transform = if parent == "0" {
            "0".to_string()
        } else {
            let (_, transform) = self.plain_object(parent)?;
            self.docs[transform].id().to_string()
        };
        let object_id = self.new_id(operation, key);
        let transform_id = self.new_id(operation, &format!("{key}.transform"));
        let mut object = base_fields();
        object.extend(json!({"serializedVersion":6,"m_Component":[{"component":{"fileID":transform_id}}],"m_Layer":0,"m_Name":name,"m_TagString":"Untagged","m_IsActive":1,"m_Icon":{"fileID":"0"},"m_NavMeshLayer":0,"m_StaticEditorFlags":0}).as_object().unwrap().clone());
        self.append_document(1, &object_id, "GameObject", &object)?;
        let mut transform = base_fields();
        transform.extend(json!({"serializedVersion":2,"m_GameObject":{"fileID":object_id},"m_LocalRotation":{"x":0,"y":0,"z":0,"w":1},"m_LocalPosition":{"x":0,"y":0,"z":0},"m_LocalScale":{"x":1,"y":1,"z":1},"m_ConstrainProportionsScale":0,"m_Children":[],"m_Father":{"fileID":parent_transform},"m_LocalEulerAnglesHint":{"x":0,"y":0,"z":0}}).as_object().unwrap().clone());
        transform.extend(fields.clone());
        let type_name = if class_id == 224 {
            "RectTransform"
        } else {
            "Transform"
        };
        self.append_document(class_id, &transform_id, type_name, &transform)?;
        self.append_child(&parent_transform, &transform_id)?;
        Ok((object_id, transform_id))
    }
    pub(crate) fn reparent(&mut self, file_id: &str, parent_id: &str) -> Result<String> {
        let (_, transform) = self.plain_object(file_id)?;
        let transform_id = self.docs[transform].id().to_string();
        self.require_resolved_subtree(&transform_id)?;
        let old_parent = id(self.docs[transform].field("m_Father"));
        let old_owner = self
            .find(&old_parent)
            .map_or_else(|| "0".to_string(), |doc| id(doc.field("m_GameObject")));
        let new_parent = if parent_id == "0" {
            "0".to_string()
        } else {
            let (_, parent) = self.plain_object(parent_id)?;
            self.docs[parent].id().to_string()
        };
        let mut seen = HashSet::new();
        let mut current = new_parent.clone();
        while current != "0" {
            if current == transform_id || !seen.insert(current.clone()) {
                return Err(failure(
                    "transform_cycle",
                    "reparent would create a Transform cycle",
                    None,
                ));
            }
            let ancestor = self
                .find(&current)
                .ok_or_else(|| failure("invalid_graph", "ancestor Transform absent", None))?;
            current = id(ancestor.field("m_Father"));
        }
        if old_parent == new_parent {
            return Ok(old_owner);
        }
        self.detach_child(&old_parent, &transform_id)?;
        self.append_child(&new_parent, &transform_id)?;
        self.set_top_level(&transform_id, "m_Father", &reference(&new_parent))?;
        Ok(old_owner)
    }
    pub(crate) fn delete_object(&mut self, file_id: &str, recursive: bool) -> Result<Vec<String>> {
        let (_, transform) = self.plain_object(file_id)?;
        let transform_id = self.docs[transform].id().to_string();
        let father = id(self.docs[transform].field("m_Father"));
        self.require_resolved_subtree(&transform_id)?;
        if !recursive && !ids_from_list(self.docs[transform].field("m_Children")).is_empty() {
            return Err(failure(
                "has_children",
                "recursive deletion must be requested for GameObjects with children",
                None,
            ));
        }
        let mut removed = BTreeSet::new();
        let mut pending = vec![file_id.to_string()];
        while let Some(object_id) = pending.pop() {
            if removed.contains(&object_id) {
                return Err(failure(
                    "transform_cycle",
                    "cycle in deleted hierarchy",
                    None,
                ));
            }
            let (object, transform) = self.plain_object(&object_id)?;
            removed.insert(object_id);
            for entry in items(self.docs[object].field("m_Component")) {
                removed.insert(id(&entry["component"]));
            }
            let mut owners = Vec::new();
            for child in ids_from_list(self.docs[transform].field("m_Children")) {
                let child = self
                    .find(&child)
                    .ok_or_else(|| failure("invalid_graph", "child Transform absent", None))?;
                owners.push(id(child.field("m_GameObject")));
            }
            pending.extend(owners.into_iter().rev());
        }
        self.detach_child(&father, &transform_id)?;
        self.docs.retain(|doc| !removed.contains(doc.id()));
        self.refresh()?;
        Ok(removed.into_iter().collect())
    }
    pub(crate) fn add_component(
        &mut self,
        operation: &str,
        key: &str,
        object_id: &str,
        script_guid: &str,
        fields: &BTreeMap<String, Value>,
    ) -> Result<String> {
        let (object, _) = self.plain_object(object_id)?;
        let mut data = base_fields();
        data.extend(json!({"m_GameObject":{"fileID":object_id},"m_Enabled":1,"m_EditorHideFlags":0,"m_Script":{"fileID":"11500000","guid":script_guid,"type":3},"m_Name":"","m_EditorClassIdentifier":""}).as_object().unwrap().clone());
        let reserved: HashSet<String> = data.keys().cloned().collect();
        for (field, value) in fields {
            if reserved.contains(field) || protected(field) || !super::script::identifier(field) {
                return Err(failure(
                    "protected_property",
                    "component fields cannot replace Unity identity or ownership fields",
                    Some(json!({"property": field})),
                ));
            }
            data.insert(field.clone(), normalize(value)?);
        }
        let component = self.new_id(operation, key);
        let mut components = items(self.docs[object].field("m_Component")).to_vec();
        self.append_document(114, &component, "MonoBehaviour", &data)?;
        components.push(json!({"component": reference(&component)}));
        self.set_top_level(object_id, "m_Component", &Value::Array(components))?;
        Ok(component)
    }
    pub(crate) fn remove_component(&mut self, component_id: &str) -> Result<()> {
        let component = self
            .find(component_id)
            .ok_or_else(|| failure("not_found", "component fileID is absent", None))?;
        let owner = id(component.field("m_GameObject"));
        if [1, 4, 224].contains(&component.class_id()) || owner == "0" {
            return Err(failure(
                "protected_property",
                "GameObject and Transform must use graph operations",
                None,
            ));
        }
        let (object, _) = self.plain_object(&owner)?;
        let remaining: Vec<Value> = items(self.docs[object].field("m_Component"))
            .iter()
            .filter(|entry| id(&entry["component"]) != component_id)
            .cloned()
            .collect();
        self.set_top_level(&owner, "m_Component", &Value::Array(remaining))?;
        self.docs.retain(|doc| doc.id() != component_id);
        self.refresh()
    }
    pub(crate) fn component_ids(&self, position: usize) -> Vec<String> {
        items(self.docs[position].field("m_Component"))
            .iter()
            .map(|entry| id(&entry["component"]))
            .collect()
    }
    pub(crate) fn set_components(&mut self, owner: &str, ids: &[String]) -> Result<()> {
        self.set_top_level(owner, "m_Component", &component_entries(ids))
    }
}
