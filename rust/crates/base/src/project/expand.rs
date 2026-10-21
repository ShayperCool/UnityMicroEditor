//! Recursive authored prefab expansion, typed overrides and structural edits.
use super::{
    lexpath,
    session::{Inner, Session, needs_import, override_key, validate_authored_path},
    strconv,
    types::{AuthoredDependency, AuthoredGraph, AuthoredGraphObject, AuthoredIdentity, Document},
    value::{self, Map, Value, array, file_id, get, map_file_id, number, object, str},
};
use anyhow::{Result, anyhow, bail};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Debug, Default)]
pub(crate) struct Expanded {
    pub class_id: i64,
    pub file_id: String,
    pub type_name: String,
    pub data: Map,
    pub stripped: bool,
    pub source_path: String,
    pub guid: String,
    pub targets: BTreeSet<String>,
    /// The local ID in the asset whose expansion last returned this object;
    /// empty when the sources do not determine it.
    pub identity: String,
    pub removed: bool,
    pub missing_script: bool,
}
impl Expanded {
    pub fn get(&self, key: &str) -> &Value {
        get(&self.data, key)
    }
}

struct Instance<'a> {
    document: &'a Document,
    objects: Vec<usize>,
    lookup: HashMap<String, usize>,
    source_guid: String,
}

pub(crate) fn transform(class: i64) -> bool {
    class == 4 || class == 224
}
pub(crate) fn target_key(guid: &str, id: &str) -> String {
    format!("{guid}/{id}")
}
pub(crate) fn builtin_guid(guid: &str) -> bool {
    guid == "0000000000000000e000000000000000" || guid == "0000000000000000f000000000000000"
}

pub(crate) fn visit_references_mut(
    value: &mut Value,
    path: &str,
    visit: &mut dyn FnMut(&str, &mut Map),
) {
    match value {
        Value::Object(map) => visit_map_mut(map, path, visit),
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                visit_references_mut(item, &format!("{path}[{index}]"), visit);
            }
        }
        _ => {}
    }
}
pub(crate) fn visit_map_mut(map: &mut Map, path: &str, visit: &mut dyn FnMut(&str, &mut Map)) {
    if map.contains_key("fileID") {
        visit(path, map);
        return;
    }
    for (key, item) in map.iter_mut() {
        if !matches!(item, Value::Object(_) | Value::Array(_)) {
            continue;
        }
        if path.is_empty() {
            visit_references_mut(item, key, visit);
        } else {
            visit_references_mut(item, &format!("{path}.{key}"), visit);
        }
    }
}

const EDITOR_KEYS: [&str; 3] = [
    "m_CorrespondingSourceObject",
    "m_PrefabInstance",
    "m_PrefabAsset",
];

fn rewrite_map(map: &Map, guid: &str, mapping: &BTreeMap<String, String>) -> Map {
    if map.contains_key("fileID") {
        let mut reference = map.clone();
        let original = map_file_id(map).into_owned();
        let target = value::text(map, "guid");
        if original != "0"
            && (target.is_empty() || target == guid)
            && let Some(id) = mapping.get(&original)
            && !id.is_empty()
        {
            reference.insert("fileID".into(), id.as_str().into());
            reference.remove("guid");
            reference.remove("type");
        }
        return reference;
    }
    map.iter()
        .filter(|(key, _)| !EDITOR_KEYS.contains(&key.as_str()))
        .map(|(key, item)| (key.clone(), rewrite_references(item, guid, mapping)))
        .collect()
}
pub(crate) fn rewrite_references(
    value: &Value,
    guid: &str,
    mapping: &BTreeMap<String, String>,
) -> Value {
    match value {
        Value::Object(map) => Value::Object(rewrite_map(map, guid, mapping)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| rewrite_references(item, guid, mapping))
                .collect(),
        ),
        other => other.clone(),
    }
}

enum Token {
    Key(String),
    Index(String),
}
/// `\.Array\.data\[(\d+)\]` → `[$1]`, then `([^\.\[\]]+)|\[(\d+)\]` tokens.
fn property_tokens(path: &str) -> Vec<Token> {
    let mut normalized = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(at) = rest.find(".Array.data[") {
        let after = &rest[at + ".Array.data[".len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && after.as_bytes().get(digits) == Some(&b']') {
            normalized.push_str(&rest[..at]);
            normalized.push('[');
            normalized.push_str(&after[..=digits]);
            rest = &after[digits + 1..];
        } else {
            normalized.push_str(&rest[..at + 1]);
            rest = &rest[at + 1..];
        }
    }
    normalized.push_str(rest);
    let bytes = normalized.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !matches!(bytes[i], b'.' | b'[' | b']') {
            let start = i;
            while i < bytes.len() && !matches!(bytes[i], b'.' | b'[' | b']') {
                i += 1;
            }
            tokens.push(Token::Key(normalized[start..i].into()));
        } else if bytes[i] == b'[' {
            let digits = bytes[i + 1..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            if digits > 0 && bytes.get(i + 1 + digits) == Some(&b']') {
                tokens.push(Token::Index(normalized[i + 1..i + 1 + digits].into()));
                i += digits + 2;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    tokens
}

fn contains_reference(reference: &Map) -> bool {
    !value::text(reference, "guid").is_empty() || map_file_id(reference) != "0"
}

/// Matches `^-?\d+$`.
fn integer_text(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn override_value(old: &Value, raw: &Value, reference: &Map) -> Result<Value> {
    let text = str(raw);
    Ok(match old {
        Value::Object(map) => {
            if !map.contains_key("fileID") {
                bail!("only scalar or reference overrides are supported");
            }
            Value::Object(reference.clone())
        }
        Value::Bool(_) => Value::Bool(text == "1" || lexpath::equal_fold(&text, "true")),
        Value::Int(_) | Value::Number(_) => {
            if text.contains(['.', 'e', 'E']) {
                Value::Float(strconv::parse_float(&text)?)
            } else {
                Value::Int(strconv::parse_int(&text, 10)?)
            }
        }
        Value::Uint(_) => Value::Uint(strconv::parse_uint(&text, 10)?),
        Value::Float(_) => Value::Float(strconv::parse_float(&text)?),
        Value::String(_) => Value::String(text.into_owned()),
        Value::Null => {
            if contains_reference(reference) {
                Value::Object(reference.clone())
            } else if raw.is_null() {
                Value::String(String::new())
            } else if integer_text(&text) {
                Value::Int(strconv::parse_int(&text, 10)?)
            } else if let Ok(parsed) = strconv::parse_float(&text) {
                Value::Float(parsed)
            } else {
                Value::String(text.into_owned())
            }
        }
        _ => bail!("only scalar or reference overrides are supported"),
    })
}

fn resize(old: &mut Value, raw: &Value) -> Result<()> {
    let Value::Array(items) = old else {
        bail!("Array.size requires an array");
    };
    let size = strconv::atoi(&str(raw))
        .ok()
        .filter(|size| (0..=100000).contains(size))
        .ok_or_else(|| anyhow!("invalid array size"))? as usize;
    let fill = items
        .last()
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    items.resize(size, fill);
    Ok(())
}

/// The typed override semantics of the runtime expander. Nothing is written
/// to the final property when the override is refused. `script`: the target
/// is a script's object (a MonoBehaviour or ScriptableObject document), whose
/// data lacks the key of a field added to the script after the prefab was
/// saved; Unity takes the override's value for it, and the absent parents of
/// the path are made (mcp6-2). A key the script does not have is left to the
/// Player, which skips it as Unity does.
pub(crate) fn apply_override(
    data: &mut Map,
    path: &str,
    raw: &Value,
    reference: &Map,
    script: bool,
) -> Result<()> {
    let is_resize = path.ends_with(".Array.size");
    let path = path.strip_suffix(".Array.size").unwrap_or(path);
    let tokens = property_tokens(path);
    let Some((last, parents)) = tokens.split_last() else {
        bail!("empty property path");
    };
    let has_index = script || tokens.iter().any(|t| matches!(t, Token::Index(_)));
    enum Cursor<'a> {
        Fields(&'a mut Map),
        Item(&'a mut Value),
    }
    let mut current = Cursor::Fields(data);
    for token in parents {
        match token {
            Token::Index(digits) => {
                let index = strconv::atoi_lossy(digits) as usize;
                let item = match current {
                    Cursor::Item(Value::Array(items)) => items.get_mut(index),
                    _ => None,
                };
                current = Cursor::Item(
                    item.ok_or_else(|| anyhow!("array index {digits} does not exist"))?,
                );
            }
            Token::Key(key) => {
                let fields = match current {
                    Cursor::Fields(fields) | Cursor::Item(Value::Object(fields)) => fields,
                    _ => bail!("{key} is not a mapping"),
                };
                if !fields.contains_key(key) {
                    if !has_index {
                        bail!("field {key} does not exist");
                    }
                    fields.insert(key.clone(), Value::Object(Map::new()));
                }
                current = Cursor::Item(fields.get_mut(key).expect("present"));
            }
        }
    }
    match last {
        Token::Index(digits) => {
            let index = strconv::atoi_lossy(digits) as usize;
            let slot = match current {
                Cursor::Item(Value::Array(items)) => items.get_mut(index),
                _ => None,
            }
            .ok_or_else(|| anyhow!("array index {digits} does not exist"))?;
            if is_resize {
                return resize(slot, raw);
            }
            *slot = override_value(slot, raw, reference)?;
        }
        Token::Key(key) => {
            let fields = match current {
                Cursor::Fields(fields) | Cursor::Item(Value::Object(fields)) => fields,
                _ => bail!("property parent is not a mapping"),
            };
            match fields.get_mut(key) {
                Some(slot) => {
                    if is_resize {
                        return resize(slot, raw);
                    }
                    *slot = override_value(slot, raw, reference)?;
                }
                None => {
                    if !has_index
                        && !(key == "m_RootOrder" && !get(fields, "m_LocalPosition").is_null())
                    {
                        bail!("field {key} does not exist");
                    }
                    if is_resize {
                        bail!("Array.size requires an array");
                    }
                    let value = override_value(&Value::Null, raw, reference)?;
                    fields.insert(key.clone(), value);
                }
            }
        }
    }
    Ok(())
}

/// Exposes the expander's override semantics for authoring validation. The map
/// is modified in memory; no file is written.
pub fn apply_authored_override(
    data: &mut Map,
    path: &str,
    value: &Value,
    reference: &Map,
) -> Result<()> {
    apply_override(data, path, value, reference, false)
}

fn reference_id<'a>(item: &'a Value, key: &str) -> std::borrow::Cow<'a, str> {
    if key.is_empty() {
        file_id(item)
    } else {
        file_id(item.field(key))
    }
}
fn filter_references(items: &[Value], key: &str, removed: &HashSet<String>) -> Vec<Value> {
    items
        .iter()
        .filter(|item| !removed.contains(reference_id(item, key).as_ref()))
        .cloned()
        .collect()
}
fn insert_reference(items: &[Value], entry: Value, key: &str, index: i64) -> Result<Vec<Value>> {
    let own = HashSet::from([reference_id(&entry, key).into_owned()]);
    let mut items = filter_references(items, key, &own);
    let index = if index == -1 {
        items.len() as i64
    } else {
        index
    };
    if index < 0 || index as usize > items.len() {
        bail!("insertion index {index} exceeds {}", items.len());
    }
    items.insert(index as usize, entry);
    Ok(items)
}
fn local_reference(id: &str) -> Value {
    Value::Object(value::map([("fileID", id.into())]))
}

fn apply_structural(
    arena: &mut [Expanded],
    instance: &Instance,
    all: &[usize],
    guid: &str,
    mapping: &BTreeMap<String, String>,
) -> Result<()> {
    let modification = object(instance.document.get("m_Modification"));
    let mut removed: HashSet<String> = HashSet::new();
    let by_id: HashMap<String, usize> = all
        .iter()
        .map(|&index| (arena[index].file_id.clone(), index))
        .collect();
    let target = |reference: &Value| -> Option<usize> {
        let fields = object(reference);
        let mut target_guid = value::text(fields, "guid");
        if target_guid.is_empty() {
            target_guid = instance.source_guid.as_str().into();
        }
        instance
            .lookup
            .get(&target_key(&target_guid, &map_file_id(fields)))
            .copied()
    };
    for reference in array(get(modification, "m_RemovedComponents")) {
        match target(reference) {
            Some(o) if arena[o].class_id != 1 && !transform(arena[o].class_id) => {
                removed.insert(arena[o].file_id.clone());
            }
            _ => bail!("invalid removed component"),
        }
    }
    for reference in array(get(modification, "m_RemovedGameObjects")) {
        let Some(o) = target(reference).filter(|&o| arena[o].class_id == 1) else {
            bail!("invalid removed GameObject");
        };
        let mut pending = vec![arena[o].file_id.clone()];
        while let Some(owner) = pending.pop() {
            if !removed.insert(owner.clone()) {
                continue;
            }
            let mut transforms = HashSet::new();
            for &c in &instance.objects {
                if file_id(arena[c].get("m_GameObject")) == owner {
                    removed.insert(arena[c].file_id.clone());
                    if transform(arena[c].class_id) {
                        transforms.insert(arena[c].file_id.clone());
                    }
                }
            }
            for &c in &instance.objects {
                if transform(arena[c].class_id)
                    && transforms.contains(file_id(arena[c].get("m_Father")).as_ref())
                {
                    pending.push(file_id(arena[c].get("m_GameObject")).into_owned());
                }
            }
        }
    }
    for field in ["m_AddedComponents", "m_AddedGameObjects"] {
        for item in array(get(modification, field)) {
            let addition = object(item);
            let parent = target(get(addition, "targetCorrespondingSourceObject"));
            let added_reference = rewrite_references(get(addition, "addedObject"), guid, mapping);
            let added = by_id.get(file_id(&added_reference).as_ref()).copied();
            let (Some(parent), Some(added)) = (parent, added) else {
                bail!("missing authored prefab addition");
            };
            if removed.contains(&arena[parent].file_id) {
                bail!("missing authored prefab addition");
            }
            let index = match get(addition, "insertIndex") {
                Value::Null => -1,
                other => number(other),
            };
            let (parent_id, added_id) =
                (arena[parent].file_id.clone(), arena[added].file_id.clone());
            let (parent_class, added_class) = (arena[parent].class_id, arena[added].class_id);
            if field == "m_AddedComponents" {
                if parent_class != 1 || added_class == 1 || transform(added_class) {
                    bail!("invalid added component");
                }
                arena[added]
                    .data
                    .insert("m_GameObject".into(), local_reference(&parent_id));
                let entry = Value::Object(value::map([("component", local_reference(&added_id))]));
                let items = insert_reference(
                    array(arena[parent].get("m_Component")),
                    entry,
                    "component",
                    index,
                )?;
                arena[parent]
                    .data
                    .insert("m_Component".into(), Value::Array(items));
            } else {
                if !transform(parent_class) || !transform(added_class) {
                    bail!("added GameObjects require Transform references");
                }
                let father = file_id(arena[added].get("m_Father")).into_owned();
                if let Some(&previous) = by_id.get(&father)
                    && previous != parent
                {
                    let children = filter_references(
                        array(arena[previous].get("m_Children")),
                        "",
                        &HashSet::from([added_id.clone()]),
                    );
                    arena[previous]
                        .data
                        .insert("m_Children".into(), Value::Array(children));
                }
                arena[added]
                    .data
                    .insert("m_Father".into(), local_reference(&parent_id));
                let items = insert_reference(
                    array(arena[parent].get("m_Children")),
                    local_reference(&added_id),
                    "",
                    index,
                )?;
                arena[parent]
                    .data
                    .insert("m_Children".into(), Value::Array(items));
            }
        }
    }
    for &index in all {
        let o = &mut arena[index];
        if removed.contains(&o.file_id) {
            o.removed = true;
            continue;
        }
        let list = if o.class_id == 1 {
            Some(("m_Component", "component"))
        } else if transform(o.class_id) {
            Some(("m_Children", ""))
        } else {
            None
        };
        if let Some((name, key)) = list {
            let filtered = filter_references(array(o.get(name)), key, &removed);
            o.data.insert(name.into(), Value::Array(filtered));
        }
        if removed.is_empty() {
            continue;
        }
        visit_map_mut(&mut o.data, "", &mut |_, reference| {
            if value::text(reference, "guid").is_empty()
                && removed.contains(map_file_id(reference).as_ref())
            {
                reference.clear();
                reference.insert("fileID".into(), "0".into());
            }
        });
    }
    Ok(())
}

impl Inner {
    pub(crate) fn document_source(&mut self, logical: &str) -> Result<Arc<Vec<Document>>> {
        self.depend(logical)?;
        let guid = self.guid_of(logical).to_string();
        if let Some(model) = self.models.get(&guid) {
            let mut documents: Vec<Document> = model.as_ref().clone();
            let local: HashSet<String> = documents.iter().map(|d| d.file_id.clone()).collect();
            let mut native: HashSet<String> = HashSet::new();
            for asset in &self.catalog.assets {
                if value::text(asset, "guid") == guid {
                    native.insert(value::text(asset, "fileId").into_owned());
                }
            }
            for asset in self.assets.values() {
                if value::text(asset, "kind") == "imported" && value::text(asset, "guid") == guid {
                    native.insert(value::text(asset, "fileId").into_owned());
                }
            }
            if let Some(replacement) = self.native_overlays.get(&guid) {
                for object in replacement.content.objects.iter() {
                    native.insert(object.file_id.clone());
                }
            }
            for document in &mut documents {
                let Some(data) = document.data.as_mut() else {
                    continue;
                };
                visit_map_mut(data, "", &mut |_, reference| {
                    let id = map_file_id(reference).into_owned();
                    if value::text(reference, "guid").is_empty()
                        && id != "0"
                        && !local.contains(&id)
                        && native.contains(&id)
                    {
                        reference.insert("guid".into(), guid.as_str().into());
                        reference.insert("type".into(), Value::Int(3));
                    }
                });
            }
            return Ok(Arc::new(documents));
        }
        let extension = lexpath::lower_ext(logical);
        if extension != ".unity" && extension != ".prefab" {
            if super::native::model_profile(&extension) {
                return Err(needs_import(logical));
            }
            bail!("native catalog lacks model graph: {logical}");
        }
        let path = self.source(logical)?;
        self.parse(&path)
    }

    /// Expands a scene or prefab into owned objects and its local ID mapping.
    pub(crate) fn expand(
        &mut self,
        logical: &str,
        prefix: &str,
        stack: &[String],
    ) -> Result<(Vec<Expanded>, BTreeMap<String, String>)> {
        let mut arena = Vec::new();
        let (indices, mapping) = self.expand_into(&mut arena, logical, prefix, stack)?;
        let objects = indices
            .into_iter()
            .map(|index| std::mem::take(&mut arena[index]))
            .collect();
        Ok((objects, mapping))
    }

    fn expand_into(
        &mut self,
        arena: &mut Vec<Expanded>,
        logical: &str,
        prefix: &str,
        stack: &[String],
    ) -> Result<(Vec<usize>, BTreeMap<String, String>)> {
        if stack.len() > 64 {
            bail!("prefab depth exceeds 64: {logical}");
        }
        if stack.iter().any(|path| path == logical) {
            bail!("recursive prefab dependency: {logical}");
        }
        let documents = self.document_source(logical)?;
        let guid = self.guid_of(logical).to_string();
        if guid.is_empty() {
            bail!("source has no GUID metadata: {logical}");
        }
        let (mut own, mut children): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        let mut own_documents: Vec<&Document> = Vec::new();
        let mut mapping: BTreeMap<String, String> = BTreeMap::new();
        for d in documents.iter() {
            if d.class_id == 1001 || d.stripped {
                continue;
            }
            let id = format!("{prefix}{}", d.file_id);
            mapping.insert(d.file_id.clone(), id.clone());
            own.push(arena.len());
            own_documents.push(d);
            arena.push(Expanded {
                class_id: d.class_id,
                file_id: id,
                type_name: d.type_name.clone(),
                source_path: logical.into(),
                guid: guid.clone(),
                targets: BTreeSet::from([target_key(&guid, &d.file_id)]),
                identity: d.file_id.clone(),
                ..Expanded::default()
            });
        }
        let mut instances: Vec<Instance> = Vec::new();
        let mut nested = stack.to_vec();
        nested.push(logical.into());
        for instance in documents.iter().filter(|d| d.class_id == 1001) {
            let source_guid = str(instance.get("m_SourcePrefab").field("guid")).into_owned();
            let source = self.path_of(&source_guid).to_string();
            if source.is_empty() {
                bail!("unresolved prefab source {source_guid} in {logical}");
            }
            let (objects, _) = self.expand_into(
                arena,
                &source,
                &format!("{prefix}{}:", instance.file_id),
                &nested,
            )?;
            let mut lookup = HashMap::new();
            for &o in &objects {
                for target in &arena[o].targets {
                    lookup.insert(target.clone(), o);
                }
            }
            children.extend(&objects);
            instances.push(Instance {
                document: instance,
                objects,
                lookup,
                source_guid,
            });
        }
        // A stripped document is the identity the Editor keeps for the object it
        // names. Two of them for one object leave it without a known identity.
        let mut placeholders: HashMap<usize, Option<&str>> = HashMap::new();
        for d in documents.iter().filter(|d| d.stripped) {
            let instance_id = file_id(d.get("m_PrefabInstance"));
            let reference = object(d.get("m_CorrespondingSourceObject"));
            let target = instances
                .iter()
                .find(|i| i.document.file_id == instance_id)
                .and_then(|i| {
                    i.lookup.get(&target_key(
                        &value::text(reference, "guid"),
                        &map_file_id(reference),
                    ))
                })
                .copied();
            let Some(target) = target else {
                bail!("cannot resolve stripped object {} in {logical}", d.file_id);
            };
            mapping.insert(d.file_id.clone(), arena[target].file_id.clone());
            arena[target].targets.insert(target_key(&guid, &d.file_id));
            placeholders
                .entry(target)
                .and_modify(|identity| *identity = None)
                .or_insert(Some(&d.file_id));
        }
        let prefab = lexpath::lower_ext(logical) == ".prefab";
        if prefab {
            // The Editor imports an inherited object under its stripped document's
            // ID, or else under the instance ID XOR the ID the object has in the
            // instance's source asset. When a document of this file already has
            // that ID, the Editor gives the object another one that the sources do
            // not determine, so the object has no identity here.
            let authored: HashSet<&str> = documents.iter().map(|d| d.file_id.as_str()).collect();
            let mut derived: HashMap<String, usize> = HashMap::new();
            for instance in &instances {
                let instance_id = strconv::parse_int(&instance.document.file_id, 10)?;
                for &o in &instance.objects {
                    if let Some(placeholder) = placeholders.get(&o) {
                        arena[o].identity = placeholder.unwrap_or_default().into();
                        continue;
                    }
                    if arena[o].identity.is_empty() {
                        continue;
                    }
                    let source = strconv::parse_int(&arena[o].identity, 10)?;
                    let id = ((instance_id ^ source) & i64::MAX).to_string();
                    if authored.contains(id.as_str()) {
                        arena[o].identity.clear();
                        continue;
                    }
                    if let Some(&other) = derived.get(&id) {
                        bail!(
                            "inherited prefab identity collision {id} in {logical}: {} ({}) and {} ({})",
                            arena[other].file_id,
                            arena[other].source_path,
                            arena[o].file_id,
                            arena[o].source_path
                        );
                    }
                    derived.insert(id.clone(), o);
                    mapping.insert(id.clone(), arena[o].file_id.clone());
                    arena[o].targets.insert(target_key(&guid, &id));
                    arena[o].identity = id;
                }
            }
        }
        for (&o, d) in own.iter().zip(&own_documents) {
            arena[o].data = rewrite_map(d.fields(), &guid, &mapping);
        }
        let all: Vec<usize> = own.iter().chain(&children).copied().collect();
        let mut instance_roots: HashMap<String, String> = HashMap::new();
        for instance in &instances {
            let modification = object(instance.document.get("m_Modification"));
            for item in array(get(modification, "m_Modifications")) {
                let change = object(item);
                let reference = object(get(change, "target"));
                let mut target_guid = value::text(reference, "guid").into_owned();
                if target_guid.is_empty() {
                    target_guid = instance.source_guid.clone();
                }
                let target_id = map_file_id(reference).into_owned();
                let property = value::text(change, "propertyPath").into_owned();
                let Some(&target) = instance.lookup.get(&target_key(&target_guid, &target_id))
                else {
                    if let Some(proof) =
                        self.proven_missing_native_model_target(&target_guid, &target_id)
                    {
                        self.record_ignored_prefab_override(
                            proof,
                            logical,
                            &instance.document.file_id,
                            &target_guid,
                            &target_id,
                            &property,
                        );
                        continue;
                    }
                    let mut missing = false;
                    if let Some(observation) = self.prefab_observations.get(&target_guid) {
                        missing = get(observation, "complete").is_true()
                            && super::native::valid_native_id(&target_id)
                            && !array(get(observation, "fileIds"))
                                .iter()
                                .any(|id| str(id) == target_id);
                    }
                    if missing {
                        continue;
                    }
                    bail!("unresolved prefab override {target_guid}/{target_id} in {logical}");
                };
                let rewritten = rewrite_references(get(change, "objectReference"), &guid, &mapping);
                let script = arena[target].class_id == 114;
                if let Err(error) = apply_override(
                    &mut arena[target].data,
                    &property,
                    get(change, "value"),
                    object(&rewritten),
                    script,
                ) {
                    if self.ignored_overrides.contains(&override_key(
                        logical,
                        &instance.document.file_id,
                        &target_guid,
                        &target_id,
                        &property,
                    )) {
                        continue;
                    }
                    return Err(error.context(format!("{logical} override {property}")));
                }
            }
            apply_structural(arena, instance, &all, &guid, &mapping)
                .map_err(|e| e.context(logical.to_string()))?;
            let roots: Vec<usize> = instance
                .objects
                .iter()
                .copied()
                .filter(|&o| {
                    !arena[o].removed
                        && transform(arena[o].class_id)
                        && file_id(arena[o].get("m_Father")) == "0"
                })
                .collect();
            if roots.len() > 1 {
                bail!("prefab {} has multiple roots", instance.source_guid);
            }
            if let Some(&root) = roots.first() {
                let parent = match get(modification, "m_TransformParent") {
                    Value::Null => local_reference("0"),
                    other => rewrite_references(other, &guid, &mapping),
                };
                arena[root].data.insert("m_Father".into(), parent);
                instance_roots.insert(
                    instance.document.file_id.clone(),
                    arena[root].file_id.clone(),
                );
            }
        }
        let result: Vec<usize> = all.iter().copied().filter(|&o| !arena[o].removed).collect();
        for &o in &own {
            if arena[o].class_id != 1660057539 {
                continue;
            }
            if let Some(Value::Array(roots)) = arena[o].data.get_mut("m_Roots") {
                for reference in roots {
                    if let Value::Object(fields) = reference
                        && let Some(root) = instance_roots.get(map_file_id(fields).as_ref())
                        && !root.is_empty()
                    {
                        fields.insert("fileID".into(), root.as_str().into());
                    }
                }
            }
        }
        if prefab {
            let roots: Vec<usize> = result
                .iter()
                .copied()
                .filter(|&o| {
                    transform(arena[o].class_id) && file_id(arena[o].get("m_Father")) == "0"
                })
                .collect();
            if roots.len() == 1 {
                let owner = file_id(arena[roots[0]].get("m_GameObject")).into_owned();
                for &o in &result {
                    if arena[o].class_id == 1 && arena[o].file_id == owner {
                        arena[o]
                            .data
                            .insert("m_Name".into(), lexpath::stem(logical).into());
                    }
                }
            }
        }
        // Missing MonoScripts may be ignored only with matching Editor observations.
        let mut missing_scripts = Vec::new();
        if let Some(observation) = self.prefab_observations.get(&guid) {
            for item in array(get(observation, "missingComponents")) {
                let missing = object(item);
                if !get(missing, "observedMissing").is_true() {
                    continue;
                }
                let owner = mapping
                    .get(value::text(missing, "gameObjectFileId").as_ref())
                    .cloned()
                    .unwrap_or_default();
                let slot = number(get(missing, "componentIndex"));
                for &o in &own {
                    if arena[o].file_id != owner {
                        continue;
                    }
                    let components = array(arena[o].get("m_Component"));
                    if slot < 0 || slot as usize >= components.len() {
                        continue;
                    }
                    let id = file_id(components[slot as usize].field("component"));
                    for &candidate in &own {
                        if arena[candidate].file_id == id && arena[candidate].class_id == 114 {
                            let script = str(arena[candidate].get("m_Script").field("guid"));
                            if self.path_of(&script).is_empty() && !builtin_guid(&script) {
                                missing_scripts.push(candidate);
                            }
                        }
                    }
                }
            }
        }
        for candidate in missing_scripts {
            arena[candidate].missing_script = true;
        }
        Ok((result, mapping))
    }

    pub(crate) fn authored_graph(&mut self, path: &str) -> Result<AuthoredGraph> {
        let extension = lexpath::ext(path);
        if validate_authored_path(path, extension).is_err()
            || extension != ".unity" && extension != ".prefab"
        {
            bail!("authored graph requires a normalized Assets scene or prefab");
        }
        self.index_guids()?;
        let dependencies = std::mem::take(&mut self.dependencies);
        let observed = std::mem::take(&mut self.cache.observed);
        let diagnostics = std::mem::take(&mut self.ignored_diagnostics);
        // A prepared Session can also inspect native-model instances through its
        // validated catalog, without publishing or loading native AssetBundle scenes.
        let result = self
            .expand(path, "", &[])
            .map(|(objects, mapping)| AuthoredGraph {
                path: path.into(),
                guid: self.guid_of(path).into(),
                objects: objects
                    .into_iter()
                    .map(|o| AuthoredGraphObject {
                        identities: {
                            let mut identities: Vec<AuthoredIdentity> = o
                                .targets
                                .iter()
                                .filter_map(|target| target.split_once('/'))
                                .map(|(guid, file_id)| AuthoredIdentity {
                                    guid: guid.into(),
                                    file_id: file_id.into(),
                                })
                                .collect();
                            identities
                                .sort_by(|a, b| (&a.guid, &a.file_id).cmp(&(&b.guid, &b.file_id)));
                            identities
                        },
                        document: Document {
                            class_id: o.class_id,
                            file_id: o.file_id,
                            type_name: o.type_name,
                            data: Some(o.data),
                            stripped: o.stripped,
                        },
                        source_path: o.source_path,
                        guid: o.guid,
                    })
                    .collect(),
                local_ids: mapping,
                dependencies: self
                    .dependencies
                    .values()
                    .map(|d| AuthoredDependency {
                        path: d.path.clone(),
                        sha256: d.sha256.clone(),
                    })
                    .collect(),
                ignored_prefab_override_diagnostics: self.sorted_ignored_diagnostics(),
            });
        self.dependencies = dependencies;
        self.cache.observed = observed;
        self.ignored_diagnostics = diagnostics;
        result
    }
}

impl Session {
    /// The recursive authored expansion, including native-model instances when
    /// this session carries a validated catalog. Nothing is published.
    pub fn authored_graph(&self, path: &str) -> Result<AuthoredGraph> {
        self.lock().authored_graph(path)
    }
}

/// Expands one authored scene or prefab of `project` without a catalog.
pub fn resolve_authored_graph(project: &str, path: &str) -> Result<AuthoredGraph> {
    let extension = lexpath::ext(path);
    if validate_authored_path(path, extension).is_err()
        || extension != ".unity" && extension != ".prefab"
    {
        bail!("authored graph requires a normalized Assets scene or prefab");
    }
    Session::authored_only(project)?.authored_graph(path)
}
