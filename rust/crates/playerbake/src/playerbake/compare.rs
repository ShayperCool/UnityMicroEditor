//! Compares two Player serialized files object by object, for checking what
//! the tool writes against what Unity's worker wrote for the same sources.
//!
//! Local IDs differ between writers, so objects are paired by a key of what
//! they are: a game object by its hierarchy path, a component by its game
//! object, class and position, a named asset by class and name. References
//! are compared by the key of the object they lead to, and streamed data
//! (`.resS`, `.resource`) by its bytes.
use super::serialized::{Read, read_with_externals};
use super::typetree::Val;
use anyhow::{Context as _, Result};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// One object of a file in comparable form.
pub struct Inspected {
    pub key: String,
    pub class: String,
    pub local_id: i64,
    pub value: Value,
}

/// The objects of a file under their keys, in the file's order.
pub fn inspect(path: &Path) -> Result<Vec<Inspected>> {
    inspect_linked(path, &HashMap::new())
}

/// The keys of a file's objects by local ID.
fn keys(path: &Path) -> Result<HashMap<i64, String>> {
    let file = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let (objects, _) = read_with_externals(&file)?;
    let values: HashMap<i64, Val> = objects
        .iter()
        .filter_map(|o| Some((o.local_id, o.decode().ok()?)))
        .collect();
    let names = Names::new(&objects, &values);
    Ok(objects
        .iter()
        .map(|o| (o.local_id, names.key(o.local_id)))
        .collect())
}

/// The objects of a file, references into the external files `linked`
/// names by path given by the key of the object they lead to.
pub fn inspect_linked(path: &Path, linked: &HashMap<String, PathBuf>) -> Result<Vec<Inspected>> {
    let file = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let (objects, mut externals) = read_with_externals(&file)?;
    // A file the tool wrote names its external files by GUID beside it.
    let names = path.with_file_name(format!(
        "{}.externals.json",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    if let Ok(text) = std::fs::read_to_string(&names) {
        let guids: Vec<String> = serde_json::from_str(&text)?;
        externals = guids
            .iter()
            .map(|guid| match guid.as_str() {
                "0000000000000000e000000000000000" => "Library/unity default resources".to_string(),
                guid => format!("ume-asset/{guid}"),
            })
            .collect();
    }
    let resolved = externals
        .iter()
        .map(|name| linked.get(name).map(|path| keys(path)).transpose())
        .collect::<Result<Vec<_>>>()?;
    let externals: Vec<(String, Option<HashMap<i64, String>>)> =
        externals.into_iter().zip(resolved).collect();
    let directory = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut values = HashMap::new();
    let mut failures = HashMap::new();
    for object in &objects {
        match object.decode() {
            Ok(value) => {
                values.insert(object.local_id, value);
            }
            Err(error) => {
                failures.insert(
                    object.local_id,
                    format!("{error} (managed types {})", object.references.len()),
                );
            }
        }
    }
    let names = Names::new(&objects, &values);
    let mut streams = Streams::new(directory);
    let mut inspected = Vec::with_capacity(objects.len());
    for object in &objects {
        let value = match values.get(&object.local_id) {
            Some(value) => normalize(value, &names, &externals, &mut streams)?,
            None => json!({"$undecodable": failures[&object.local_id]}),
        };
        inspected.push(Inspected {
            key: names.key(object.local_id),
            class: names.class(object.local_id),
            local_id: object.local_id,
            value,
        });
    }
    Ok(inspected)
}

/// How two files differ.
#[derive(Default)]
pub struct Difference {
    pub matched: usize,
    pub equal: usize,
    pub only_ours: Vec<String>,
    pub only_theirs: Vec<String>,
    /// Per class: objects matched and equal.
    pub classes: BTreeMap<String, (usize, usize)>,
    /// Per class: the differing objects and the first differing fields.
    pub fields: BTreeMap<String, Vec<(String, Vec<String>)>>,
}

/// Compares two files, references into the external files `links` names
/// (each with our file and theirs) by the objects they lead to.
pub fn compare(
    ours: &Path,
    theirs: &Path,
    tolerance: f64,
    links: &[(String, PathBuf, PathBuf)],
) -> Result<Difference> {
    let side = |pick: fn(&(String, PathBuf, PathBuf)) -> &PathBuf| -> HashMap<String, PathBuf> {
        links
            .iter()
            .map(|link| (link.0.clone(), pick(link).clone()))
            .collect()
    };
    let ours = inspect_linked(ours, &side(|link| &link.1))?;
    let theirs = inspect_linked(theirs, &side(|link| &link.2))?;
    let mut by_key: HashMap<&str, &Inspected> =
        theirs.iter().map(|o| (o.key.as_str(), o)).collect();
    let mut difference = Difference::default();
    for object in &ours {
        let Some(other) = by_key.remove(object.key.as_str()) else {
            difference.only_ours.push(object.key.clone());
            continue;
        };
        difference.matched += 1;
        let mut found = Vec::new();
        differ("", &object.value, &other.value, tolerance, &mut found);
        let class = difference.classes.entry(object.class.clone()).or_default();
        class.0 += 1;
        if found.is_empty() {
            class.1 += 1;
            difference.equal += 1;
        } else {
            difference
                .fields
                .entry(object.class.clone())
                .or_default()
                .push((object.key.clone(), found));
        }
    }
    difference.only_theirs = theirs
        .iter()
        .filter(|o| by_key.contains_key(o.key.as_str()))
        .map(|o| o.key.clone())
        .collect();
    Ok(difference)
}

const MAX_FIELDS: usize = 12;

fn differ(path: &str, a: &Value, b: &Value, tolerance: f64, out: &mut Vec<String>) {
    if out.len() >= MAX_FIELDS {
        return;
    }
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            for (name, value) in a {
                let at = join(path, name);
                match b.get(name) {
                    Some(other) => differ(&at, value, other, tolerance, out),
                    None => out.push(format!("{at}: only ours")),
                }
            }
            for name in b.keys().filter(|name| !a.contains_key(*name)) {
                out.push(format!("{}: only theirs", join(path, name)));
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                differ(&format!("{path}[{index}]"), x, y, tolerance, out);
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            out.push(format!("{path}: {} elements != {}", a.len(), b.len()))
        }
        (Value::Number(x), Value::Number(y)) if tolerance > 0.0 => {
            let (x, y) = (x.as_f64().unwrap_or(0.0), y.as_f64().unwrap_or(0.0));
            if (x - y).abs() > tolerance {
                out.push(format!("{path}: {x} != {y}"));
            }
        }
        _ if a != b => out.push(format!("{path}: {} != {}", short(a), short(b))),
        _ => {}
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

fn short(value: &Value) -> String {
    let text = value.to_string();
    if text.len() > 80 {
        format!("{}…", &text[..80])
    } else {
        text
    }
}

/// Keys and class names of the objects of one file.
struct Names {
    keys: HashMap<i64, String>,
    classes: HashMap<i64, String>,
}

impl Names {
    fn new(objects: &[Read], values: &HashMap<i64, Val>) -> Self {
        let field = |id: i64, name: &str| values.get(&id).and_then(|v| v.field(name)).cloned();
        let pointer = |value: Option<Val>| -> Option<(i64, i64)> {
            let value = value?;
            Some((
                int(value.field("m_FileID")?)?,
                int(value.field("m_PathID")?)?,
            ))
        };
        let name_of = |id: i64| match field(id, "m_Name") {
            Some(Val::Str(name)) => name,
            Some(Val::Bytes(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
            _ => String::new(),
        };
        let mut classes = HashMap::new();
        for object in objects {
            let mut class = object.tree.nodes[0].type_name.clone();
            if let Some(script) = object
                .script
                .filter(|_| object.script_file == 0)
                .or_else(|| {
                    pointer(field(object.local_id, "m_Script"))
                        .filter(|p| p.0 == 0)
                        .map(|p| p.1)
                })
            {
                let class_name = match field(script, "m_ClassName") {
                    Some(Val::Str(name)) => name,
                    Some(Val::Bytes(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
                    _ => String::new(),
                };
                if !class_name.is_empty() {
                    class = format!("{class}<{class_name}>");
                }
            }
            classes.insert(object.local_id, class);
        }
        // Game object of each transform, parent transform of each transform.
        let mut transform_of = HashMap::new();
        let mut parent = HashMap::new();
        for object in objects {
            if let (Some(owner), Some(father)) = (
                pointer(field(object.local_id, "m_GameObject")),
                pointer(field(object.local_id, "m_Father")),
            ) {
                transform_of.insert(owner.1, object.local_id);
                if father.0 == 0 && father.1 != 0 {
                    parent.insert(object.local_id, father.1);
                }
            }
        }
        let owner_of: HashMap<i64, i64> = transform_of.iter().map(|(go, t)| (*t, *go)).collect();
        let path_of = |go: i64| {
            let mut parts = vec![name_of(go)];
            let mut transform = transform_of.get(&go).copied();
            while let Some(up) = transform.and_then(|t| parent.get(&t).copied()) {
                parts.push(owner_of.get(&up).map(|g| name_of(*g)).unwrap_or_default());
                transform = Some(up);
            }
            parts.reverse();
            parts.join("/")
        };
        // A component's position among its game object's components.
        let mut position_of: HashMap<i64, usize> = HashMap::new();
        for object in objects {
            if let Some(Val::Seq(components)) = field(object.local_id, "m_Component") {
                for (position, entry) in components.iter().enumerate() {
                    if let Some((0, id)) = pointer(entry.field("component").cloned()) {
                        position_of.insert(id, position);
                    }
                }
            }
        }
        let mut ordered: Vec<&Read> = objects.iter().collect();
        // Components by their position on their game object; assets of the
        // same class and name by what they hold, so that equal ones take the
        // same key in both files.
        let fingerprint_of = |id: i64| values.get(&id).map(fingerprint).unwrap_or_default();
        ordered.sort_by_key(|o| {
            (
                position_of.get(&o.local_id).copied().unwrap_or(usize::MAX),
                match pointer(field(o.local_id, "m_GameObject")) {
                    Some(_) => 0,
                    None => fingerprint_of(o.local_id),
                },
            )
        });
        let mut keys = HashMap::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for object in ordered {
            let id = object.local_id;
            let class = &classes[&id];
            let base = if class == "GameObject" {
                format!("GameObject:{}", path_of(id))
            } else if let Some(owner) =
                pointer(field(id, "m_GameObject")).filter(|p| p.0 == 0 && p.1 != 0)
            {
                format!("{}#{class}", path_of(owner.1))
            } else {
                let name = name_of(id);
                if name.is_empty() {
                    class.clone()
                } else {
                    format!("{class}:{name}")
                }
            };
            let count = seen.entry(base.clone()).or_default();
            let key = if *count == 0 {
                base.clone()
            } else {
                format!("{base}[{count}]")
            };
            *count += 1;
            keys.insert(id, key);
        }
        Names { keys, classes }
    }

    fn key(&self, id: i64) -> String {
        self.keys
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("missing:{id}"))
    }

    fn class(&self, id: i64) -> String {
        self.classes.get(&id).cloned().unwrap_or_default()
    }
}

/// A hash of a value without its references and the places of its streamed
/// data, which differ between two files holding the same object.
fn fingerprint(value: &Val) -> u128 {
    fn feed(value: &Val, hasher: &mut xxhash_rust::xxh3::Xxh3) {
        match value {
            Val::Zero => hasher.update(&[0]),
            Val::Bool(b) => hasher.update(&[1, *b as u8]),
            Val::I(v) => hasher.update(&v.to_le_bytes()),
            Val::U(v) => hasher.update(&v.to_le_bytes()),
            Val::F(v) => hasher.update(&v.to_le_bytes()),
            Val::D(v) => hasher.update(&v.to_le_bytes()),
            Val::Str(text) => hasher.update(text.as_bytes()),
            Val::Bytes(bytes) => hasher.update(bytes),
            Val::Seq(items) => {
                hasher.update(&(items.len() as u64).to_le_bytes());
                for item in items {
                    feed(item, hasher);
                }
            }
            Val::Map(fields) => {
                for (name, field) in fields {
                    if matches!(name.as_ref(), "m_FileID" | "m_PathID" | "m_StreamData") {
                        continue;
                    }
                    hasher.update(name.as_bytes());
                    feed(field, hasher);
                }
            }
        }
    }
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    feed(value, &mut hasher);
    hasher.digest128()
}

fn int(value: &Val) -> Option<i64> {
    match value {
        Val::I(n) => Some(*n),
        Val::U(n) => Some(*n as i64),
        _ => None,
    }
}

/// Streamed data files beside the inspected file, read once.
struct Streams {
    directory: PathBuf,
    files: HashMap<String, Vec<u8>>,
}

impl Streams {
    fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            files: HashMap::new(),
        }
    }

    fn hash(&mut self, path: &str, offset: u64, size: u64) -> Value {
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !self.files.contains_key(&name) {
            let data = std::fs::read(self.directory.join(&name)).unwrap_or_default();
            self.files.insert(name.clone(), data);
        }
        let data = &self.files[&name];
        match data.get(offset as usize..(offset + size) as usize) {
            // `UME_DUMP_STREAMS=1` gives where the data lies instead.
            Some(_) if std::env::var_os("UME_DUMP_STREAMS").is_some() => {
                json!({"file": self.directory.join(&name), "offset": offset, "size": size})
            }
            Some(bytes) => {
                json!({"$stream": format!("{:032x}", xxhash_rust::xxh3::xxh3_128(bytes)), "size": size})
            }
            None => json!({"$stream": "unreadable", "size": size}),
        }
    }
}

fn normalize(
    value: &Val,
    names: &Names,
    externals: &[(String, Option<HashMap<i64, String>>)],
    streams: &mut Streams,
) -> Result<Value> {
    Ok(match value {
        Val::Zero => Value::Null,
        Val::Bool(b) => json!(b),
        Val::I(n) => json!(n),
        Val::U(n) => json!(n),
        Val::F(f) => json!(f),
        Val::D(d) => json!(d),
        Val::Str(text) => json!(text),
        Val::Bytes(bytes) => match std::str::from_utf8(bytes) {
            Ok(text)
                if bytes.len() <= 256
                    && !text
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t') =>
            {
                json!(text)
            }
            // `UME_DUMP_FULL_BYTES=1` prints every byte string whole.
            _ if bytes.len() <= 128 || std::env::var_os("UME_DUMP_FULL_BYTES").is_some() => {
                json!({"$hex": bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()})
            }
            _ => {
                json!({"$bytes": format!("{:032x}", xxhash_rust::xxh3::xxh3_128(bytes)), "size": bytes.len()})
            }
        },
        Val::Seq(items) => Value::Array(
            items
                .iter()
                .map(|item| normalize(item, names, externals, streams))
                .collect::<Result<_>>()?,
        ),
        Val::Map(fields) => {
            let get = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v);
            if let (2, Some(file), Some(path)) = (
                fields.len(),
                get("m_FileID").and_then(int),
                get("m_PathID").and_then(int),
            ) {
                return Ok(match (file, path) {
                    (_, 0) => Value::Null,
                    (0, id) => json!({"$ref": names.key(id)}),
                    (n, id) => match externals.get(n as usize - 1) {
                        Some((file, Some(keys))) => match keys.get(&id) {
                            Some(key) => json!({"$ref": format!("{file}:{key}")}),
                            None => json!({"$ref": format!("{file}:{id}")}),
                        },
                        Some((file, None)) => json!({"$ref": format!("{file}:{id}")}),
                        None => json!({"$ref": format!(":{id}")}),
                    },
                });
            }
            let text = |name: &str| match get(name) {
                Some(Val::Str(text)) => Some(text.clone()),
                Some(Val::Bytes(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => None,
            };
            let number = |name: &str| get(name).and_then(int).map(|n| n as u64);
            for [path, offset, size] in [
                ["path", "offset", "size"],
                ["m_Source", "m_Offset", "m_Size"],
            ] {
                if let (3, Some(path), Some(offset), Some(size)) =
                    (fields.len(), text(path), number(offset), number(size))
                    && !path.is_empty()
                {
                    return Ok(streams.hash(&path, offset, size));
                }
            }
            let mut map = Map::new();
            for (name, item) in fields {
                map.insert(
                    name.to_string(),
                    normalize(item, names, externals, streams)?,
                );
            }
            Value::Object(map)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playerbake::serialized::{Object, write};

    fn file(names: &[&str], directory: &Path, label: &str) -> PathBuf {
        let tree = crate::playerbake::script_tree().unwrap();
        let objects: Vec<Object<'_>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let mut value = tree.decode(&tree.encode(&Val::Zero).unwrap()).unwrap();
                *value.field_mut("m_Name").unwrap() = Val::str(name);
                *value.field_mut("m_ExecutionOrder").unwrap() = Val::I(index as i64);
                Object {
                    local_id: 10 + index as i64,
                    tree,
                    data: tree.encode(&value).unwrap(),
                    script: None,
                    script_file: 0,
                }
            })
            .collect();
        let path = directory.join(label);
        std::fs::write(&path, write(&objects, &[]).unwrap()).unwrap();
        path
    }

    #[test]
    fn objects_are_paired_by_name_whatever_their_local_ids() {
        let directory = std::env::temp_dir().join(format!("ume-compare-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let ours = file(&["A", "B"], &directory, "ours");
        let theirs = file(&["A", "B"], &directory, "theirs");
        let same = compare(&ours, &theirs, 0.0, &[]).unwrap();
        assert_eq!((same.matched, same.equal), (2, 2));
        // B moves to position 0: both objects keep their keys, and the
        // execution order each was given differs.
        let swapped = file(&["B", "A", "C"], &directory, "swapped");
        let difference = compare(&swapped, &theirs, 0.0, &[]).unwrap();
        assert_eq!((difference.matched, difference.equal), (2, 0));
        assert_eq!(difference.only_ours, ["MonoScript:C"]);
        let fields = &difference.fields["MonoScript"][0].1;
        assert_eq!(fields, &["m_ExecutionOrder: 0 != 1"]);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
