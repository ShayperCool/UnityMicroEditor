//! Prefab instances unpacked into the objects they stand for, as a build
//! does before it writes a scene or a prefab.
//!
//! An object of an instance takes the file ID `(instance ^ source) & 2^63-1`
//! in the file that holds the instance, where `source` is its file ID in the
//! (itself unpacked) source prefab; the stripped stand-ins of the file carry
//! the same IDs. The instance's modifications set fields by property path,
//! its removed components and game objects are left out, its root is hung
//! under the instance's parent, and the components and game objects the file
//! adds are listed by their owners.
use super::authored::{Reference, Sources};
use anyhow::{Context as _, Result, bail};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use ume_base::yaml::Document;

const PREFAB_INSTANCE: i64 = 1001;
const MASK: i64 = i64::MAX;

/// Source prefabs already unpacked, by GUID.
#[derive(Default)]
pub struct Unpacked {
    prefabs: HashMap<String, Arc<Vec<Document>>>,
    pending: HashSet<String>,
}

fn id_of(document: &Document) -> Result<i64> {
    document
        .file_id
        .parse()
        .with_context(|| format!("file ID {}", document.file_id))
}

fn instance_id(instance: i64, source: i64) -> i64 {
    (instance ^ source) & MASK
}

fn reference(fields: &Map<String, Value>, name: &str) -> Option<Reference> {
    fields.get(name).and_then(Reference::from_value)
}

/// Points the references a file makes to its own stripped objects at the
/// objects they stand for.
fn redirect(value: &mut Value, stand_ins: &HashMap<i64, i64>) {
    match value {
        Value::Object(fields) => {
            let own = fields
                .get("guid")
                .is_none_or(|guid| guid.as_str() == Some(""));
            if own
                && let Some(Value::String(id)) = fields.get_mut("fileID")
                && let Some(target) = id.parse::<i64>().ok().and_then(|id| stand_ins.get(&id))
            {
                *id = target.to_string();
                return;
            }
            for field in fields.values_mut() {
                redirect(field, stand_ins);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| redirect(item, stand_ins)),
        _ => {}
    }
}

fn local(file_id: i64) -> Value {
    let mut fields = Map::new();
    fields.insert("fileID".into(), Value::String(file_id.to_string()));
    Value::Object(fields)
}

impl Unpacked {
    /// The documents of a file with its prefab instances unpacked.
    pub fn unpack(&mut self, sources: &Sources, documents: Vec<Document>) -> Result<Vec<Document>> {
        if !documents.iter().any(|d| d.class_id == PREFAB_INSTANCE) {
            return Ok(documents);
        }
        let documents: Vec<Document> = documents
            .into_iter()
            .filter(|d| {
                d.class_id != PREFAB_INSTANCE
                    || d.data.get("m_IsPrefabParent").and_then(Value::as_str) != Some("1")
            })
            .collect();
        let mut own: Vec<Document> = Vec::new();
        let mut instances = Vec::new();
        // A stripped object stands for the object of an instance it
        // corresponds to: references to it go to that object.
        let mut stand_ins: HashMap<i64, i64> = HashMap::new();
        for document in documents.iter().filter(|d| d.stripped) {
            if let (Some(instance), Some(source)) = (
                reference(&document.data, "m_PrefabInstance"),
                reference(&document.data, "m_CorrespondingSourceObject"),
            ) {
                stand_ins.insert(
                    id_of(document)?,
                    instance_id(instance.file_id, source.file_id),
                );
            }
        }
        for document in documents {
            // The prefab record of the format before nested prefabs marks
            // the asset itself and stands for nothing.
            let legacy = document
                .data
                .get("m_IsPrefabParent")
                .and_then(Value::as_str)
                == Some("1");
            if document.class_id == PREFAB_INSTANCE && !document.stripped && !legacy {
                instances.push(document);
            } else if !document.stripped {
                own.push(document);
            }
        }
        if !stand_ins.is_empty() {
            for document in own.iter_mut().chain(instances.iter_mut()) {
                for value in document.data.values_mut() {
                    redirect(value, &stand_ins);
                }
            }
        }
        // An instance of a prefab the project lacks is a missing prefab,
        // which a build leaves out: references to its objects are empty.
        instances.retain(|instance| {
            reference(&instance.data, "m_SourcePrefab")
                .is_none_or(|source| source.guid.is_empty() || sources.path(&source.guid).is_some())
        });
        let mut merged: Vec<Document> = Vec::new();
        for instance in &instances {
            merged.extend(self.instance(sources, instance)?);
        }
        let mut by_id: HashMap<i64, usize> = HashMap::new();
        for (position, document) in merged.iter().enumerate() {
            by_id.insert(id_of(document)?, position);
        }
        // Components and children the file adds to objects of an instance.
        let mut added: Vec<(i64, &'static str, &'static str, i64, i64)> = Vec::new();
        for instance in &instances {
            let modification = instance
                .data
                .get("m_Modification")
                .and_then(Value::as_object);
            let instance_file_id = id_of(instance)?;
            for (list, field, item) in [
                ("m_AddedComponents", "m_Component", "component"),
                ("m_AddedGameObjects", "m_Children", ""),
            ] {
                for entry in modification
                    .and_then(|m| m.get(list))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_object)
                {
                    let (Some(target), Some(object)) = (
                        reference(entry, "targetCorrespondingSourceObject"),
                        reference(entry, "addedObject"),
                    ) else {
                        continue;
                    };
                    let index = entry
                        .get("insertIndex")
                        .and_then(Value::as_str)
                        .and_then(|i| i.parse().ok())
                        .unwrap_or(-1);
                    added.push((
                        instance_id(instance_file_id, target.file_id),
                        field,
                        item,
                        object.file_id,
                        index,
                    ));
                }
            }
        }
        // An added game object is listed by its transform, which the entry
        // names.
        for &(owner, field, item, object, index) in &added {
            let Some(&position) = by_id.get(&owner) else {
                continue;
            };
            let entry = if item.is_empty() {
                local(object)
            } else {
                Value::Object([(item.to_string(), local(object))].into_iter().collect())
            };
            let list = merged[position]
                .data
                .entry(field.to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(items) = list {
                let index = if index < 0 || index as usize > items.len() {
                    items.len()
                } else {
                    index as usize
                };
                items.insert(index, entry);
            }
        }
        // Older files list no added objects: an own component of an instance's
        // game object, and an own transform under an instance's transform,
        // are added at the end.
        for document in &own {
            let id = id_of(document)?;
            let (owner, field, entry) = if let Some(father) = reference(&document.data, "m_Father")
            {
                (father.file_id, "m_Children", local(id))
            } else if let Some(owner) = reference(&document.data, "m_GameObject") {
                (
                    owner.file_id,
                    "m_Component",
                    Value::Object([("component".to_string(), local(id))].into_iter().collect()),
                )
            } else {
                continue;
            };
            let Some(&position) = by_id.get(&owner) else {
                continue;
            };
            if let Some(Value::Array(items)) = merged[position].data.get_mut(field)
                && !items.contains(&entry)
            {
                items.push(entry);
            }
        }
        own.extend(merged);
        // A transform is among the children of its parent: an instance put
        // under an object of another instance (in a variant) is listed by
        // neither file, and goes at the end.
        let positions: HashMap<i64, usize> = own
            .iter()
            .enumerate()
            .filter_map(|(position, document)| Some((id_of(document).ok()?, position)))
            .collect();
        let mut missing: Vec<(usize, i64)> = Vec::new();
        for document in own.iter().filter(|d| d.class_id == 4 || d.class_id == 224) {
            let Some(father) = reference(&document.data, "m_Father").filter(|f| f.file_id != 0)
            else {
                continue;
            };
            let Some(&position) = positions.get(&father.file_id) else {
                continue;
            };
            let id = id_of(document)?;
            let listed = match own[position].data.get("m_Children") {
                Some(Value::Array(children)) => children
                    .iter()
                    .filter_map(Reference::from_value)
                    .any(|child| child.file_id == id),
                _ => true,
            };
            if !listed {
                missing.push((position, id));
            }
        }
        for (position, id) in missing {
            if let Some(Value::Array(children)) = own[position].data.get_mut("m_Children") {
                children.push(local(id));
            }
        }
        Ok(own)
    }

    fn prefab(&mut self, sources: &Sources, guid: &str) -> Result<Arc<Vec<Document>>> {
        if let Some(prefab) = self.prefabs.get(guid) {
            return Ok(prefab.clone());
        }
        if !self.pending.insert(guid.to_string()) {
            bail!("prefab {guid} contains itself");
        }
        let path = sources
            .path(guid)
            .with_context(|| format!("no source prefab with GUID {guid}"))?;
        let documents = ume_base::yaml::parse_unity_yaml_lexical(
            &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
        )?;
        let unpacked = Arc::new(self.unpack(sources, documents)?);
        self.pending.remove(guid);
        self.prefabs.insert(guid.to_string(), unpacked.clone());
        Ok(unpacked)
    }

    /// The objects of one instance, in the file ID space of its file.
    fn instance(&mut self, sources: &Sources, instance: &Document) -> Result<Vec<Document>> {
        let file_id = id_of(instance)?;
        let source = reference(&instance.data, "m_SourcePrefab")
            .filter(|r| !r.guid.is_empty())
            .context("a prefab instance names no source prefab")?;
        let prefab = self.prefab(sources, &source.guid)?;
        let modification = instance
            .data
            .get("m_Modification")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let targets = |list: &str| -> HashSet<i64> {
            modification
                .get(list)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Reference::from_value)
                .map(|r| r.file_id)
                .collect()
        };
        let mut removed = targets("m_RemovedComponents");
        let removed_objects = targets("m_RemovedGameObjects");
        if !removed_objects.is_empty() {
            // A removed game object takes its components and children along.
            let mut pending: Vec<i64> = removed_objects.into_iter().collect();
            while let Some(object) = pending.pop() {
                if !removed.insert(object) {
                    continue;
                }
                for document in prefab.iter() {
                    let id = id_of(document)?;
                    if reference(&document.data, "m_GameObject").map(|r| r.file_id) == Some(object)
                    {
                        pending.push(id);
                        if let Some(Value::Array(children)) = document.data.get("m_Children") {
                            for child in children.iter().filter_map(Reference::from_value) {
                                if let Some(owner) = prefab
                                    .iter()
                                    .find(|d| d.file_id == child.file_id.to_string())
                                    && let Some(go) = reference(&owner.data, "m_GameObject")
                                {
                                    pending.push(go.file_id);
                                }
                            }
                        }
                    }
                }
            }
        }
        let parent = reference(&modification, "m_TransformParent")
            .map(|r| r.file_id)
            .unwrap_or(0);
        let mut documents = Vec::with_capacity(prefab.len());
        for document in prefab.iter() {
            let source_id = id_of(document)?;
            if removed.contains(&source_id) {
                continue;
            }
            let mut copy = document.clone();
            copy.file_id = instance_id(file_id, source_id).to_string();
            let mut data = Value::Object(std::mem::take(&mut copy.data));
            renumber(&mut data, file_id, &removed);
            let Value::Object(mut data) = data else {
                unreachable!()
            };
            if data.contains_key("m_Father")
                && reference(&data, "m_Father").is_none_or(|r| r.file_id == 0)
            {
                data.insert("m_Father".into(), local(parent));
            }
            copy.data = data;
            documents.push(copy);
        }
        let positions: HashMap<String, usize> = documents
            .iter()
            .enumerate()
            .map(|(position, d)| (d.file_id.clone(), position))
            .collect();
        for change in modification
            .get("m_Modifications")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
        {
            let Some(target) = reference(change, "target") else {
                continue;
            };
            let Some(&position) = positions.get(&instance_id(file_id, target.file_id).to_string())
            else {
                continue;
            };
            let Some(path) = change.get("propertyPath").and_then(Value::as_str) else {
                continue;
            };
            let mut object = change
                .get("objectReference")
                .cloned()
                .unwrap_or(Value::Null);
            // An object of the source prefab is, in the instance, its copy.
            if let Some(reference) = Reference::from_value(&object)
                && reference.guid == source.guid
            {
                let id = if removed.contains(&reference.file_id) {
                    0
                } else {
                    instance_id(file_id, reference.file_id)
                };
                object = local(id);
            }
            let value = change.get("value").cloned().unwrap_or(Value::Null);
            let document = &mut documents[position];
            set_path(&mut document.data, path, value, object);
        }
        Ok(documents)
    }
}

/// Every reference to an object of the same source prefab, renumbered into
/// the instance; a reference to a removed object becomes empty.
fn renumber(value: &mut Value, instance: i64, removed: &HashSet<i64>) {
    match value {
        Value::Object(fields) => {
            if let Some(reference) = Reference::from_value(&Value::Object(fields.clone()))
                && fields.contains_key("fileID")
            {
                if reference.guid.is_empty() && reference.file_id != 0 {
                    let id = if removed.contains(&reference.file_id) {
                        0
                    } else {
                        instance_id(instance, reference.file_id)
                    };
                    fields.insert("fileID".into(), Value::String(id.to_string()));
                }
                return;
            }
            for field in fields.values_mut() {
                renumber(field, instance, removed);
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| renumber(item, instance, removed)),
        _ => {}
    }
}

/// Sets the field at a property path (`a.b`, `list.Array.data[2].c`,
/// `list.Array.size`): a reference when the modification names an object or
/// the field holds one, the authored text otherwise.
fn set_path(data: &mut Map<String, Value>, path: &str, value: Value, object: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut slot: &mut Value = match data.get_mut(parts[0]) {
        Some(slot) => slot,
        None => {
            if parts.len() > 1 {
                return;
            }
            data.insert(parts[0].to_string(), Value::Null);
            data.get_mut(parts[0]).unwrap()
        }
    };
    let mut rest = &parts[1..];
    while let Some(part) = rest.first() {
        if *part == "Array" {
            let Some(next) = rest.get(1) else { return };
            if *next == "size" {
                let size: usize = value.as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
                if !slot.is_array() {
                    *slot = Value::Array(Vec::new());
                }
                if let Value::Array(items) = slot {
                    let filler = items.last().cloned().unwrap_or(Value::Null);
                    items.resize(size, filler);
                }
                return;
            }
            let Some(index) = next
                .strip_prefix("data[")
                .and_then(|i| i.strip_suffix(']'))
                .and_then(|i| i.parse::<usize>().ok())
            else {
                return;
            };
            if !slot.is_array() {
                *slot = Value::Array(Vec::new());
            }
            let Value::Array(items) = slot else { return };
            if index >= items.len() {
                let filler = items.last().cloned().unwrap_or(Value::Null);
                items.resize(index + 1, filler);
            }
            slot = &mut items[index];
            rest = &rest[2..];
            continue;
        }
        if !slot.is_object() {
            *slot = Value::Object(Map::new());
        }
        let Value::Object(fields) = slot else { return };
        slot = fields.entry(part.to_string()).or_insert(Value::Null);
        rest = &rest[1..];
    }
    let names_object = Reference::from_value(&object).is_some_and(|r| r.file_id != 0);
    let holds_reference = slot.as_object().is_some_and(|f| f.contains_key("fileID"));
    *slot = if names_object || holds_reference {
        object
    } else {
        value
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_property_path_reaches_into_sequences() {
        let mut data: Map<String, Value> = serde_json::from_str(
            r#"{"m_Materials": [{"fileID": "1"}], "m_LocalPosition": {"x": "0", "y": "0"}, "list": ["a"]}"#,
        )
        .unwrap();
        let reference: Value =
            serde_json::from_str(r#"{"fileID": "7", "guid": "g", "type": "2"}"#).unwrap();
        set_path(
            &mut data,
            "m_Materials.Array.data[0]",
            Value::String(String::new()),
            reference.clone(),
        );
        set_path(
            &mut data,
            "m_LocalPosition.y",
            Value::String("2.5".into()),
            Value::Null,
        );
        set_path(
            &mut data,
            "list.Array.size",
            Value::String("3".into()),
            Value::Null,
        );
        set_path(
            &mut data,
            "list.Array.data[2]",
            Value::String("c".into()),
            Value::Null,
        );
        assert_eq!(data["m_Materials"][0], reference);
        assert_eq!(data["m_LocalPosition"]["y"], "2.5");
        assert_eq!(data["list"], serde_json::json!(["a", "a", "c"]));
    }

    #[test]
    fn objects_of_an_instance_take_the_ids_of_their_stripped_stand_ins() {
        assert_eq!(
            instance_id(6858707258670286937, 1099559309614460715),
            5795410016486074226
        );
        let mut data: Value = serde_json::from_str(
            r#"{"m_GameObject": {"fileID": "1099559309614460715"}, "m_Sprite": {"fileID": "21300000", "guid": "abc", "type": "3"}, "gone": {"fileID": "5"}}"#,
        )
        .unwrap();
        renumber(&mut data, 6858707258670286937, &[5].into_iter().collect());
        assert_eq!(data["m_GameObject"]["fileID"], "5795410016486074226");
        assert_eq!(data["m_Sprite"]["fileID"], "21300000");
        assert_eq!(data["gone"]["fileID"], "0");
    }
}
