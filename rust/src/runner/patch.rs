//! The incremental scene patch a live Player can apply.
use super::{noop::normalize_scene_dependencies, read_file};
use crate::{
    jsondoc,
    project::{Map, Value, value},
    sha256, sysutil,
    util::atomic_json,
};
use anyhow::{Result, anyhow, bail};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, BTreeSet};

pub const SCENE_ROOTS_CLASS: i64 = 1660057539;

static NULL: Value = Value::Null;

struct PatchScene {
    fields: Map,
    revision: String,
    /// Records in authored order with their identities.
    objects: Vec<(String, Map)>,
    by_id: BTreeMap<String, usize>,
    data: BTreeMap<String, Map>,
}
impl PatchScene {
    fn record(&self, id: &str) -> Option<&Map> {
        self.by_id.get(id).map(|&index| &self.objects[index].1)
    }
}

fn field<'a>(map: &'a Map, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&NULL)
}

/// Decodes a whole document, keeping number literals verbatim.
pub(crate) fn parse_document(data: &[u8]) -> Result<Value> {
    let (value, used) = value::parse_json(data, true)?;
    match data[used..]
        .iter()
        .find(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        None => Ok(value),
        Some(&byte) => bail!(
            "invalid character {:?} after top-level value",
            char::from(byte)
        ),
    }
}

/// One value, numbers verbatim, nothing after it.
pub fn decode_patch_json(data: &[u8]) -> Result<Value> {
    let (value, used) = value::parse_json(data, true)?;
    if !jsondoc::rest_is_space(data, used) {
        bail!("trailing JSON data");
    }
    Ok(value)
}

/// The exact integer of a JSON number, `-1` for anything else.
pub fn patch_integer(value: &Value) -> i64 {
    match value {
        Value::Number(text) => value::decimal(text)
            .and_then(|number| number.int64())
            .unwrap_or(-1),
        _ => -1,
    }
}

/// The `fileID` string of a reference map.
pub fn patch_reference(value: &Value) -> &str {
    match value {
        Value::Object(map) => field(map, "fileID").as_str().unwrap_or(""),
        _ => "",
    }
}

/// Numbers compare by exact rational value; a literal that is not an exact
/// decimal only equals the same literal.
pub fn patch_value_equal(old: &Value, next: &Value) -> bool {
    match (old, next) {
        (Value::Number(a), Value::Number(b)) => match (value::decimal(a), value::decimal(b)) {
            (Some(a), Some(b)) => a == b,
            (None, None) => a == b,
            _ => false,
        },
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| patch_value_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => patch_maps_equal(a, b),
        _ => old == next,
    }
}

/// Deep equality of two normalised maps: an absent key is not a `null` one.
fn patch_maps_equal(old: &Map, next: &Map) -> bool {
    old.len() == next.len()
        && old.iter().all(|(key, item)| {
            next.get(key)
                .is_some_and(|other| patch_value_equal(item, other))
        })
}

/// Here an absent key does equal `null`.
fn patch_maps_equal_except(old: &Map, next: &Map, ignored: &[&str]) -> bool {
    old.iter().all(|(key, item)| {
        ignored.contains(&key.as_str()) || patch_value_equal(item, field(next, key))
    }) && next.iter().all(|(key, item)| {
        ignored.contains(&key.as_str()) || patch_value_equal(item, field(old, key))
    })
}

fn patch_record_equal(old: &Map, next: &Map, old_data: &Map, next_data: &Map) -> bool {
    patch_maps_equal_except(old, next, &["dataJson", "scalarDataJson", "references"])
        && patch_maps_equal(old_data, next_data)
}

/// Whether a manifest names a scene in the Player format.
fn native_scene(path: &str) -> Result<bool> {
    Ok(match parse_document(&read_file(path)?)? {
        Value::Object(fields) => matches!(fields.get("nativeScene"), Some(Value::Object(_))),
        _ => false,
    })
}

fn read_patch_scene(path: &str) -> Result<PatchScene> {
    let fields = match parse_document(&read_file(path)?)? {
        Value::Object(fields) => fields,
        Value::Null => Map::new(),
        other => bail!(
            "json: cannot unmarshal {} into value of type object",
            kind(&other)
        ),
    };
    let revision = match field(&fields, "revision") {
        Value::String(revision) if !revision.is_empty() => revision.clone(),
        _ => bail!("scene revision is missing"),
    };
    let malformed = || anyhow!("scene objects are missing or malformed");
    let records = match fields.get("objects") {
        Some(Value::Array(records)) if !records.is_empty() => records,
        _ => return Err(malformed()),
    };
    if records
        .iter()
        .any(|record| !matches!(record, Value::Object(_) | Value::Null))
    {
        return Err(malformed());
    }
    let mut scene = PatchScene {
        revision,
        objects: Vec::with_capacity(records.len()),
        by_id: BTreeMap::new(),
        data: BTreeMap::new(),
        fields: Map::new(),
    };
    for record in records {
        let empty = Map::new();
        let record = record.as_object().unwrap_or(&empty);
        let id = match field(record, "fileId") {
            Value::String(id) if !id.is_empty() && !scene.by_id.contains_key(id) => id.clone(),
            _ => bail!("scene object identities must be unique nonempty strings"),
        };
        if patch_integer(field(record, "classId")) <= 0 {
            bail!("object {id} classId is invalid");
        }
        let Value::String(text) = field(record, "dataJson") else {
            bail!("object {id} dataJson is invalid");
        };
        let Ok(Value::Object(data)) = decode_patch_json(text.as_bytes()) else {
            bail!("object {id} dataJson must be a map");
        };
        scene.by_id.insert(id.clone(), scene.objects.len());
        scene.data.insert(id.clone(), data);
        scene.objects.push((id, record.clone()));
    }
    scene.fields = fields;
    Ok(scene)
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        _ => "number",
    }
}

/// An absent field is treated like a `null` one.
fn patch_scene_dependencies(fields: &Map) -> Value {
    let mut value = field(fields, "sourceDependencies").clone();
    normalize_scene_dependencies(&mut value);
    value
}

/// Records are visited in identity order, so the fault reported among several is
/// deterministic.
fn validate_patch_graph(scene: &PatchScene) -> Result<()> {
    let mut transforms: BTreeMap<&str, &Map> = BTreeMap::new();
    let mut root_ids: BTreeSet<&str> = BTreeSet::new();
    let mut root_list: &[Value] = &[];
    let mut root_metadata = 0;
    for (id, &index) in &scene.by_id {
        let record = &scene.objects[index].1;
        let class = patch_integer(field(record, "classId"));
        let data = &scene.data[id];
        if class == 4 || class == 224 {
            transforms.insert(id, data);
            let parent = patch_reference(field(data, "m_Father"));
            if parent.is_empty() {
                bail!("Transform {id} parent descriptor is missing");
            }
            if parent == "0" {
                root_ids.insert(id);
            }
            let owner = field(record, "gameObjectId").as_str().unwrap_or("");
            if scene
                .record(owner)
                .is_none_or(|owner| patch_integer(field(owner, "classId")) != 1)
                || patch_reference(field(data, "m_GameObject")) != owner
            {
                bail!("Transform {id} GameObject identity differs");
            }
        }
        if class == 1 {
            let layer = field(data, "m_Layer");
            if !layer.is_null() && !(0..=31).contains(&patch_integer(layer)) {
                bail!("GameObject {id} layer is invalid");
            }
            let active = field(data, "m_IsActive");
            if !active.is_null() && !matches!(patch_integer(active), 0 | 1) {
                bail!("GameObject {id} active state is invalid");
            }
        }
        if class == SCENE_ROOTS_CLASS {
            root_metadata += 1;
            let Value::Array(roots) = field(data, "m_Roots") else {
                bail!("SceneRoots list is missing");
            };
            root_list = roots;
        }
    }
    let father = |id: &str| {
        transforms
            .get(id)
            .map_or("", |data| patch_reference(field(data, "m_Father")))
    };
    let mut listed_children: BTreeMap<&str, &str> = BTreeMap::new();
    for (&id, data) in &transforms {
        let mut parent = patch_reference(field(data, "m_Father"));
        if parent != "0" && !transforms.contains_key(parent) {
            bail!("Transform {id} parent is missing");
        }
        let Value::Array(children) = field(data, "m_Children") else {
            bail!("Transform {id} child list is missing");
        };
        let mut seen = BTreeSet::new();
        for reference in children {
            let child = patch_reference(reference);
            if child.is_empty()
                || seen.contains(child)
                || !transforms.contains_key(child)
                || father(child) != id
            {
                bail!("Transform {id} child/parent records disagree");
            }
            seen.insert(child);
            listed_children.insert(child, id);
        }
        let mut visited = BTreeSet::from([id]);
        while parent != "0" {
            if !visited.insert(parent) {
                bail!("Transform cycle");
            }
            parent = father(parent);
        }
    }
    for (&id, data) in &transforms {
        let parent = patch_reference(field(data, "m_Father"));
        if parent != "0" && listed_children.get(id).copied().unwrap_or("") != parent {
            bail!("Transform {id} is absent from its parent's child list");
        }
    }
    if root_metadata > 1 {
        bail!("multiple SceneRoots records");
    }
    if root_metadata == 1 {
        let mut seen = BTreeSet::new();
        for reference in root_list {
            if let Value::Object(map) = reference
                && field(map, "guid")
                    .as_str()
                    .is_some_and(|guid| !guid.is_empty())
            {
                bail!("SceneRoots requires local authored references");
            }
            let id = patch_reference(reference);
            if !root_ids.contains(id) || !seen.insert(id) {
                bail!("SceneRoots list does not match root Transforms");
            }
        }
        if seen.len() != root_ids.len() {
            bail!("SceneRoots omits a root Transform");
        }
    }
    Ok(())
}

/// Equality over the decoded top-level members: absence only equals absence.
fn json_equal(old: Option<&Value>, next: Option<&Value>) -> bool {
    match (old, next) {
        (Some(old), Some(next)) => patch_value_equal(old, next),
        (None, None) => true,
        _ => false,
    }
}

/// Preserves unrelated runtime state. Script/component
/// changes, removals, reparenting and changed native content use the complete
/// loader: `None` then. Otherwise the published patch path and its SHA256.
pub fn make_scene_patch(previous: &str, next: &str) -> Result<Option<(String, String)>> {
    // A scene the engine loads itself holds no object records to patch; it
    // is loaded again.
    if native_scene(previous)? || native_scene(next)? {
        return Ok(None);
    }
    let old = read_patch_scene(previous)?;
    let fresh = read_patch_scene(next)?;
    if old.revision == fresh.revision {
        bail!("scene patch requires a new revision");
    }
    if patch_scene_dependencies(&old.fields) != patch_scene_dependencies(&fresh.fields) {
        return Ok(None);
    }
    const IGNORED: [&str; 5] = [
        "objects",
        "revision",
        "sourceDependencies",
        "contentHash",
        "manifest",
    ];
    for key in old.fields.keys().chain(fresh.fields.keys()) {
        if !IGNORED.contains(&key.as_str())
            && !json_equal(old.fields.get(key), fresh.fields.get(key))
        {
            return Ok(None);
        }
    }
    if old.by_id.keys().any(|id| !fresh.by_id.contains_key(id)) {
        return Ok(None);
    }
    validate_patch_graph(&fresh)?;
    let mut changed: Vec<Value> = Vec::new();
    for (id, record) in &fresh.objects {
        let before = old.record(id);
        let data = &fresh.data[id];
        if let Some(before) = before
            && patch_record_equal(before, record, &old.data[id], data)
        {
            continue;
        }
        let class = patch_integer(field(record, "classId"));
        if class != 1 && class != 4 && class != SCENE_ROOTS_CLASS {
            return Ok(None);
        }
        if let Some(before) = before {
            for name in [
                "classId",
                "gameObjectId",
                "sourcePath",
                "guid",
                "typeName",
                "skipMissingScript",
            ] {
                if !patch_value_equal(field(before, name), field(record, name)) {
                    return Ok(None);
                }
            }
            let old_data = &old.data[id];
            let allowed: &[&str] = if class == 4 {
                &[
                    "m_LocalPosition",
                    "m_LocalRotation",
                    "m_LocalScale",
                    "m_LocalEulerAnglesHint",
                    "m_Children",
                ]
            } else if class == SCENE_ROOTS_CLASS {
                // Native Player SetSiblingIndex does not reorder scene roots.
                // Appending new roots preserves enumeration; insertion/reordering
                // needs reconstruction in the new authored creation order.
                let previous_roots = value::array(field(old_data, "m_Roots"));
                let next_roots = value::array(field(data, "m_Roots"));
                if next_roots.len() < previous_roots.len()
                    || !previous_roots
                        .iter()
                        .zip(next_roots)
                        .all(|(old, next)| patch_value_equal(old, next))
                {
                    return Ok(None);
                }
                &["m_Roots"]
            } else {
                &["m_Name", "m_IsActive", "m_Layer", "m_TagString"]
            };
            if !patch_maps_equal_except(old_data, data, allowed) {
                return Ok(None);
            }
        } else if class == 1 {
            let components = match field(data, "m_Component") {
                Value::Array(components) if components.len() == 1 => components,
                _ => return Ok(None),
            };
            let component = match &components[0] {
                Value::Object(entry) => patch_reference(field(entry, "component")),
                _ => "",
            };
            let Some(transform) = fresh.record(component) else {
                return Ok(None);
            };
            if old.by_id.contains_key(component)
                || patch_integer(field(transform, "classId")) != 4
                || field(transform, "gameObjectId").as_str() != Some(id.as_str())
            {
                return Ok(None);
            }
        } else if class == 4 {
            let owner = field(record, "gameObjectId").as_str().unwrap_or("");
            if old.by_id.contains_key(owner)
                || fresh
                    .record(owner)
                    .is_none_or(|owner| patch_integer(field(owner, "classId")) != 1)
            {
                return Ok(None);
            }
        }
        changed.push(Value::Object(record.clone()));
    }
    if changed.is_empty() {
        return Ok(None);
    }
    let patch = value::map([
        ("schemaVersion", Value::Int(1)),
        ("baseRevision", old.revision.as_str().into()),
        ("revision", fresh.revision.as_str().into()),
        ("objects", Value::Array(changed)),
    ]);
    let payload = value::compact_map(&patch)?;
    if payload.len() > 2 << 20 {
        return Ok(None);
    }
    let path = sysutil::join(&[&sysutil::dir(next), "patch.json"]);
    atomic_json(&path, &RawValue::from_string(payload)?)?;
    let content = read_file(&path)?;
    Ok(Some((path, sha256(&content))))
}
