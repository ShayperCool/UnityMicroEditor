//! Player objects written from authored Unity YAML, as a build writes them,
//! without Unity's import worker.
//!
//! An object is laid out by the Player type tree of its class (or of its
//! script's class). An authored field is converted by the type the tree
//! gives it; a field the Player has and the source lacks is one a build
//! derives, and takes the value of a sample object of the same class written
//! by the engine; an Editor-only field is dropped. What a build changes on
//! the way (a tag name becomes its index, a cached sorting layer value is
//! cleared) is settled per class afterwards.
use super::{
    managers::guid_words,
    serialized::read_with_externals,
    text_values::{hex_items, scalar},
    typetree::{TypeTree, Val},
};
use anyhow::{Context as _, Result, bail};
use serde_json::Value;
use std::{borrow::Cow, collections::HashMap, path::Path, sync::Arc};
use ume_base::yaml::Document;

/// The layout of a class and an object of it as the engine wrote one.
#[derive(Clone)]
pub struct Kind {
    pub tree: Arc<TypeTree>,
    pub sample: Val,
}

/// Type trees and samples of one engine release, read from Player files the
/// engine wrote with type trees.
#[derive(Default)]
pub struct Library {
    classes: HashMap<i32, Kind>,
    /// By the full name of the script class (`Namespace.Class`).
    scripts: HashMap<String, Kind>,
    /// The `MonoScript` object of each script class.
    script_objects: HashMap<String, Val>,
}

impl Library {
    pub fn read(paths: &[&Path]) -> Result<Self> {
        let mut library = Library::default();
        for path in paths {
            let file = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            library.add(&file)?;
        }
        Ok(library)
    }

    pub fn add(&mut self, file: &[u8]) -> Result<()> {
        let (objects, _) = read_with_externals(file)?;
        let mut names = HashMap::new();
        for object in objects.iter().filter(|o| o.tree.class_id == MONO_SCRIPT) {
            let value = object.tree.decode(&object.data)?;
            let name = full_name(
                &text_of(value.field("m_Namespace")),
                &text_of(value.field("m_ClassName")),
            );
            names.insert(object.local_id, name.clone());
            self.script_objects.entry(name).or_insert(value);
        }
        for object in &objects {
            let Ok(sample) = object.tree.decode(&object.data) else {
                continue;
            };
            let kind = Kind {
                tree: object.tree.clone(),
                sample,
            };
            match object.script.and_then(|script| names.get(&script)) {
                Some(name) => {
                    self.scripts.entry(name.clone()).or_insert(kind);
                }
                None => {
                    self.classes.entry(object.tree.class_id).or_insert(kind);
                }
            }
        }
        Ok(())
    }

    pub fn class(&self, class_id: i32) -> Option<&Kind> {
        self.classes.get(&class_id)
    }

    pub fn script(&self, full_name: &str) -> Option<&Kind> {
        self.scripts.get(full_name)
    }

    pub fn script_object(&self, full_name: &str) -> Option<&Val> {
        self.script_objects.get(full_name)
    }
}

pub const MONO_SCRIPT: i32 = 115;
const GAME_OBJECT: i32 = 1;

pub fn full_name(namespace: &str, class: &str) -> String {
    if namespace.is_empty() {
        class.to_string()
    } else {
        format!("{namespace}.{class}")
    }
}

fn text_of(value: Option<&Val>) -> String {
    match value {
        Some(Val::Str(text)) => text.clone(),
        Some(Val::Bytes(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        _ => String::new(),
    }
}

/// An authored reference: `{fileID, guid, type}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub file_id: i64,
    /// Empty for an object of the same source file.
    pub guid: String,
    pub kind: i64,
}

impl Reference {
    pub fn from_value(source: &Value) -> Option<Self> {
        let fields = source.as_object()?;
        let number = |name: &str| -> Option<i64> {
            match fields.get(name)? {
                Value::String(text) => text.parse().ok(),
                Value::Number(number) => number.as_i64(),
                _ => None,
            }
        };
        Some(Reference {
            file_id: number("fileID")?,
            guid: fields
                .get("guid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            kind: number("type").unwrap_or(0),
        })
    }
}

/// The tags a build numbers: the built-in ones, then the project's own from
/// 20000 on, in the order of `ProjectSettings/TagManager.asset`.
pub struct Tags {
    project: Vec<String>,
}

impl Tags {
    const BUILT_IN: [(&'static str, i64); 7] = [
        ("Untagged", 0),
        ("Respawn", 1),
        ("Finish", 2),
        ("EditorOnly", 3),
        ("MainCamera", 5),
        ("Player", 6),
        ("GameController", 7),
    ];

    pub fn new(project: Vec<String>) -> Self {
        Tags { project }
    }

    /// The tags of a project's `TagManager.asset`.
    pub fn of_project(project: &Path) -> Result<Self> {
        let path = project.join("ProjectSettings/TagManager.asset");
        let documents = ume_base::yaml::parse_unity_yaml_lexical(
            &std::fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )?;
        let tags = documents
            .iter()
            .find(|document| document.type_name == "TagManager")
            .and_then(|document| document.data.get("tags"))
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .map(|tag| tag.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        Ok(Tags::new(tags))
    }

    pub fn index(&self, name: &str) -> Result<i64> {
        if let Some((_, index)) = Self::BUILT_IN.iter().find(|(tag, _)| *tag == name) {
            return Ok(*index);
        }
        match self.project.iter().position(|tag| tag == name) {
            Some(position) => Ok(20000 + position as i64),
            None => bail!("tag {name:?} is not in the project's tag manager"),
        }
    }
}

/// An object converted from its document.
pub struct Converted {
    pub tree: Arc<TypeTree>,
    pub value: Val,
    /// Fields the source lacks, taken from the engine's sample object.
    pub derived: Vec<String>,
}

/// Where a reference leads in the written files: the file index of the
/// pointer (0 for the same file) and the object's local ID there.
pub type Resolve<'a> = dyn FnMut(&Reference) -> Result<Option<(i32, i64)>> + 'a;

/// Converts one document of a source. `script` is the full class name of a
/// `MonoBehaviour`'s script.
pub fn convert(
    library: &Library,
    tags: &Tags,
    document: &Document,
    script: Option<&str>,
    resolve: &mut Resolve<'_>,
) -> Result<Converted> {
    let kind = match script {
        Some(name) => library
            .script(name)
            .with_context(|| format!("no type tree of the script class {name}"))?,
        None => library
            .class(document.class_id as i32)
            .with_context(|| format!("no type tree of class {}", document.type_name))?,
    };
    let mut walk = Walk {
        tree: &kind.tree,
        resolve,
        derived: Vec::new(),
    };
    let source = Value::Object(document.data.clone());
    let mut value = walk.value(0, kind.sample.clone(), &source, "")?;
    let derived = walk.derived;
    settle(document, tags, &mut value)?;
    Ok(Converted {
        tree: kind.tree.clone(),
        value,
        derived,
    })
}

/// What a build changes in an object beyond its fields' types.
fn settle(document: &Document, tags: &Tags, value: &mut Val) -> Result<()> {
    if document.class_id == GAME_OBJECT as i64 {
        let tag = document
            .data
            .get("m_TagString")
            .and_then(Value::as_str)
            .unwrap_or("Untagged");
        if let Some(field) = value.field_mut("m_Tag") {
            *field = Val::U(tags.index(tag)? as u64);
        }
        // A component the source no longer holds is left out.
        if let Some(Val::Seq(components)) = value.field_mut("m_Component") {
            components.retain(|entry| {
                entry
                    .field("component")
                    .and_then(|pointer| pointer.field("m_PathID"))
                    .is_some_and(|id| *id != Val::I(0))
            });
        }
    }
    // The Editor caches a renderer's sorting layer value; the Player looks
    // it up by the layer's ID and a build writes 0.
    if let Some(field) = value.field_mut("m_SortingLayer") {
        *field = Val::I(0);
    }
    Ok(())
}

struct Walk<'t, 'r, 'a> {
    tree: &'t TypeTree,
    resolve: &'r mut Resolve<'a>,
    derived: Vec<String>,
}

impl Walk<'_, '_, '_> {
    fn derive(&mut self, sample: Val, path: &str) -> Val {
        self.derived.push(path.to_string());
        sample
    }

    fn value(&mut self, index: usize, sample: Val, source: &Value, path: &str) -> Result<Val> {
        let node = &self.tree.nodes[index];
        let children = self.tree.children(index);
        if children.is_empty() {
            if node.size == 0 {
                return Ok(sample);
            }
            let text = match source {
                Value::String(text) => text.as_str(),
                Value::Null => return Ok(self.derive(sample, path)),
                other => bail!("{path}: expected a scalar, found {other}"),
            };
            return scalar(&node.type_name, text, path);
        }
        if node.type_name == "string" {
            return Ok(match source {
                Value::Null => Val::Str(String::new()),
                Value::String(text) => Val::Str(text.clone()),
                other => bail!("{path}: expected a string, found {other}"),
            });
        }
        if node.type_name == "GUID" {
            let Some(words) = source.as_str().and_then(guid_words) else {
                bail!("{path}: not a GUID");
            };
            let fields = children
                .iter()
                .zip(words)
                .map(|(&child, word)| {
                    (
                        Cow::Owned(self.tree.nodes[child].name.clone()),
                        Val::U(word as u64),
                    )
                })
                .collect();
            return Ok(Val::Map(fields));
        }
        if node.type_name.starts_with("PPtr<") {
            let reference = Reference::from_value(source)
                .with_context(|| format!("{path}: expected a reference"))?;
            let (file, local) = if reference.file_id == 0 {
                (0, 0)
            } else {
                (self.resolve)(&reference)
                    .with_context(|| path.to_string())?
                    .unwrap_or((0, 0))
            };
            return Ok(Val::Map(vec![
                (Cow::Borrowed("m_FileID"), Val::I(file as i64)),
                (Cow::Borrowed("m_PathID"), Val::I(local)),
            ]));
        }
        if let Some((_, element)) = self.tree.element(index) {
            let items = match source {
                Value::Null => return Ok(Val::Seq(Vec::new())),
                Value::String(dump) if self.tree.children(element).is_empty() => {
                    let item = &self.tree.nodes[element].type_name;
                    return Ok(Val::Seq(hex_items(item, dump, path)?));
                }
                Value::Array(items) => items,
                // A map the Editor writes as one mapping of its entries.
                Value::Object(entries) => &entries
                    .iter()
                    .map(|(key, value)| {
                        Value::Object(
                            [
                                ("first".to_string(), Value::String(key.clone())),
                                ("second".to_string(), value.clone()),
                            ]
                            .into_iter()
                            .collect(),
                        )
                    })
                    .collect::<Vec<_>>(),
                other => bail!("{path}: expected a sequence, found {other}"),
            };
            let mut values = Vec::with_capacity(items.len());
            for (position, item) in items.iter().enumerate() {
                let item_path = format!("{path}[{position}]");
                // A map is written as a sequence of one-entry mappings.
                let item = match (&self.tree.nodes[element].type_name[..], item) {
                    ("pair", Value::Object(entry))
                        if entry.len() == 1 && !entry.contains_key("first") =>
                    {
                        let (key, value) = entry.iter().next().unwrap();
                        Value::Object(
                            [
                                ("first".to_string(), Value::String(key.clone())),
                                ("second".to_string(), value.clone()),
                            ]
                            .into_iter()
                            .collect(),
                        )
                    }
                    _ => item.clone(),
                };
                values.push(self.value(element, Val::Zero, &item, &item_path)?);
            }
            return Ok(Val::Seq(values));
        }
        let fields = match source {
            Value::Object(fields) => fields,
            Value::Null => return Ok(self.derive(sample, path)),
            other => bail!("{path}: expected a mapping, found {other}"),
        };
        let mut samples = match sample {
            Val::Map(fields) => fields,
            _ => Vec::new(),
        };
        let mut values = Vec::with_capacity(children.len());
        for child in children {
            let name = self.tree.nodes[child].name.clone();
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}.{name}")
            };
            let sample = samples
                .iter()
                .position(|(key, _)| *key == name)
                .map(|position| samples.swap_remove(position).1)
                .unwrap_or(Val::Zero);
            let value = match fields.get(&name) {
                // Typeless data is written as its size, its bytes follow
                // as one hexadecimal run under `_typelessdata`.
                Some(Value::String(_)) if self.tree.nodes[child].type_name == "TypelessData" => {
                    let dump = fields
                        .get("_typelessdata")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    Val::Bytes(
                        hex_items("UInt8", dump, &child_path)?
                            .into_iter()
                            .map(|item| match item {
                                Val::U(byte) => byte as u8,
                                _ => 0,
                            })
                            .collect(),
                    )
                }
                Some(field) => self.value(child, sample, field, &child_path)?,
                None => self.derive(sample, &child_path),
            };
            values.push((Cow::Owned(name), value));
        }
        Ok(Val::Map(values))
    }
}

/// The source files of a project by GUID, from their `.meta` files under
/// `Assets`, `Packages` and the package cache.
pub struct Sources {
    paths: HashMap<String, std::path::PathBuf>,
}

impl Sources {
    pub fn of_project(project: &Path) -> Result<Self> {
        let mut paths = HashMap::new();
        let mut pending: Vec<std::path::PathBuf> = ["Assets", "Packages", "Library/PackageCache"]
            .iter()
            .map(|root| project.join(root))
            .collect();
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() || (kind.is_symlink() && path.is_dir()) {
                    pending.push(path);
                } else if path.extension().is_some_and(|e| e == "meta") {
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    if let Some(guid) = text
                        .lines()
                        .find_map(|line| line.strip_prefix("guid: "))
                        .map(str::trim)
                    {
                        paths.insert(guid.to_string(), path.with_extension(""));
                    }
                }
            }
        }
        Ok(Sources { paths })
    }

    pub fn path(&self, guid: &str) -> Option<&Path> {
        self.paths.get(guid).map(std::path::PathBuf::as_path)
    }

    /// The full class name of the script a `.cs` file defines: the class
    /// named as the file, in the first namespace the file declares.
    pub fn script_class(&self, guid: &str) -> Option<String> {
        let path = self.path(guid)?;
        let class = path.file_stem()?.to_string_lossy().into_owned();
        let text = std::fs::read_to_string(path).ok()?;
        let namespace = text
            .lines()
            .map(str::trim_start)
            .find_map(|line| line.strip_prefix("namespace "))
            .map(|rest| {
                rest.trim_end_matches(|c: char| c == '{' || c == ';' || c.is_whitespace())
                    .to_string()
            })
            .unwrap_or_default();
        Some(full_name(&namespace, &class))
    }
}

/// One source file written as a Player file: its objects, and a `MonoScript`
/// for each script its behaviours use. References into other sources become
/// external references, one per GUID in order of first use.
pub struct Written {
    pub file: Vec<u8>,
    pub externals: Vec<String>,
    pub derived: Vec<(String, Vec<String>)>,
}

pub fn write_source(
    library: &Library,
    tags: &Tags,
    sources: &Sources,
    source: &Path,
    format: super::FileFormat,
) -> Result<Written> {
    let documents = ume_base::yaml::parse_unity_yaml_lexical(
        &std::fs::read(source).with_context(|| format!("read {}", source.display()))?,
    )?;
    let mut local = HashMap::new();
    for (position, document) in documents.iter().filter(|d| !d.stripped).enumerate() {
        local.insert(document.file_id.parse::<i64>()?, position as i64 + 1);
    }
    let mut scripts: Vec<String> = Vec::new();
    let mut externals: Vec<String> = Vec::new();
    let mut converted = Vec::new();
    let mut derived = Vec::new();
    for document in documents.iter().filter(|d| !d.stripped) {
        let script = match document
            .data
            .get("m_Script")
            .and_then(Reference::from_value)
        {
            Some(reference) if document.class_id == 114 => {
                let name = sources
                    .script_class(&reference.guid)
                    .with_context(|| format!("no script with GUID {}", reference.guid))?;
                if !scripts.contains(&name) {
                    scripts.push(name.clone());
                }
                Some(name)
            }
            _ => None,
        };
        let script_base = local.len() as i64 + 1;
        let mut resolve = |reference: &Reference| -> Result<Option<(i32, i64)>> {
            if reference.guid.is_empty() {
                return Ok(local.get(&reference.file_id).map(|id| (0, *id)));
            }
            if document.class_id == 114
                && document
                    .data
                    .get("m_Script")
                    .and_then(Reference::from_value)
                    .as_ref()
                    == Some(reference)
            {
                let name = script.as_deref().unwrap_or_default();
                let position = scripts.iter().position(|s| s == name).unwrap_or(0);
                return Ok(Some((0, script_base + position as i64)));
            }
            let position = match externals.iter().position(|guid| *guid == reference.guid) {
                Some(position) => position,
                None => {
                    externals.push(reference.guid.clone());
                    externals.len() - 1
                }
            };
            Ok(Some((position as i32 + 1, reference.file_id)))
        };
        let object = convert(library, tags, document, script.as_deref(), &mut resolve)
            .with_context(|| format!("{} &{}", document.type_name, document.file_id))?;
        derived.push((
            format!("{} &{}", document.type_name, document.file_id),
            object.derived.clone(),
        ));
        converted.push((local[&document.file_id.parse::<i64>()?], object, script));
    }
    let script_tree = library
        .class(MONO_SCRIPT)
        .context("no type tree of MonoScript")?
        .tree
        .clone();
    let script_base = local.len() as i64 + 1;
    let mut objects = Vec::new();
    for (position, name) in scripts.iter().enumerate() {
        let value = library
            .script_object(name)
            .with_context(|| format!("no MonoScript of {name}"))?;
        objects.push(super::Object {
            local_id: script_base + position as i64,
            tree: &script_tree,
            data: script_tree.encode(value)?,
            script: None,
        });
    }
    for (local_id, object, script) in &converted {
        objects.push(super::Object {
            local_id: *local_id,
            tree: &object.tree,
            data: object.tree.encode(&object.value)?,
            script: script.as_ref().map(|name| {
                script_base + scripts.iter().position(|s| s == name).unwrap_or(0) as i64
            }),
        });
    }
    let external_refs: Vec<super::External> = externals
        .iter()
        .map(|guid| super::External { guid: guid.clone() })
        .collect();
    Ok(Written {
        file: super::write_in(format, &objects, &external_refs)?,
        externals,
        derived,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playerbake::serialized::{Object, write};

    #[test]
    fn tags_are_numbered_as_a_build_numbers_them() {
        let tags = Tags::new(vec!["Enemy".into(), "Pickup".into()]);
        assert_eq!(tags.index("Untagged").unwrap(), 0);
        assert_eq!(tags.index("MainCamera").unwrap(), 5);
        assert_eq!(tags.index("Pickup").unwrap(), 20001);
        assert!(tags.index("Missing").is_err());
    }

    #[test]
    fn authored_fields_are_converted_and_missing_ones_derived_from_the_sample() {
        let tree = crate::playerbake::script_tree().unwrap();
        let mut sample = tree.decode(&tree.encode(&Val::Zero).unwrap()).unwrap();
        *sample.field_mut("m_AssemblyName").unwrap() = Val::str("Sample.dll");
        let file = write(
            &[Object {
                local_id: 1,
                tree,
                data: tree.encode(&sample).unwrap(),
                script: None,
            }],
            &[],
        )
        .unwrap();
        let mut library = Library::default();
        library.add(&file).unwrap();
        let document = &ume_base::yaml::parse_unity_yaml_lexical(
            b"--- !u!115 &5\nMonoScript:\n  m_Name: Probe\n  m_ExecutionOrder: -3\n  m_ClassName: Probe\n  m_EditorOnly: 1\n",
        )
        .unwrap()[0];
        let mut resolve = |_: &Reference| -> Result<Option<(i32, i64)>> { Ok(None) };
        let converted = convert(
            &library,
            &Tags::new(Vec::new()),
            document,
            None,
            &mut resolve,
        )
        .unwrap();
        assert_eq!(converted.value.field("m_Name"), Some(&Val::str("Probe")));
        assert_eq!(converted.value.field("m_ExecutionOrder"), Some(&Val::I(-3)));
        // The Player field the source lacks keeps the engine's value; the
        // Editor-only field is not written.
        assert_eq!(
            converted.value.field("m_AssemblyName"),
            Some(&Val::str("Sample.dll"))
        );
        assert!(
            converted
                .derived
                .iter()
                .any(|field| field == "m_AssemblyName")
        );
        assert!(converted.value.field("m_EditorOnly").is_none());
    }
}
