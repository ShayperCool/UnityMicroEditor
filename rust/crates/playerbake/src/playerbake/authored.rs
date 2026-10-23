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
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};
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
    /// Classes held by managed reference, by class name, namespace and
    /// assembly.
    managed: HashMap<(String, String, String), Kind>,
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

    /// The engine classes of the library as a Player file of one object each,
    /// a release's pack: the samples keep their numbers, and short lists of
    /// them (the defaults of the fields a source lacks, such as a renderer's
    /// trail streams), and lose what an object holds of its own — strings,
    /// references, longer lists and byte strings.
    pub fn to_pack(&self, format: super::serialized::FileFormat) -> Result<Vec<u8>> {
        fn plain(value: &Val) -> bool {
            match value {
                Val::Str(_) => false,
                Val::Seq(items) => items.iter().all(plain),
                Val::Map(fields) => fields
                    .iter()
                    .all(|(name, field)| name != "m_PathID" && plain(field)),
                _ => true,
            }
        }
        fn clear(value: &mut Val) {
            match value {
                Val::Str(text) => text.clear(),
                Val::Bytes(bytes) if bytes.len() > 64 => bytes.clear(),
                Val::Seq(items) if items.len() > 32 || !items.iter().all(plain) => items.clear(),
                Val::Map(fields) => {
                    let pointer = fields.iter().any(|(name, _)| name == "m_PathID");
                    for (_, field) in fields {
                        if pointer {
                            *field = match field {
                                Val::I(_) => Val::I(0),
                                Val::U(_) => Val::U(0),
                                _ => Val::Zero,
                            };
                        } else {
                            clear(field);
                        }
                    }
                }
                _ => {}
            }
        }
        let mut classes: Vec<(&i32, &Kind)> = self
            .classes
            .iter()
            .filter(|(class, _)| **class != super::typetree::SCRIPT_CLASS)
            .collect();
        classes.sort_by_key(|(class, _)| **class);
        let mut objects = Vec::new();
        for (index, (class, kind)) in classes.iter().enumerate() {
            let mut sample = kind.sample.clone();
            clear(&mut sample);
            let data = kind
                .tree
                .encode(&sample)
                .with_context(|| format!("encode the sample of class {class}"))?;
            objects.push(super::serialized::Object {
                local_id: index as i64 + 1,
                tree: &kind.tree,
                data,
                script: None,
                script_file: 0,
            });
        }
        super::serialized::write_in(format, &objects, &[])
    }

    /// The pack the tool ships for a release (`to_pack`), if any.
    pub fn of_release(unity_version: &str) -> Option<Result<Self>> {
        let pack: &'static [u8] = match unity_version {
            "6000.0.71f1" => include_bytes!("engine/6000.0.71f1/library.bin"),
            _ => return None,
        };
        let mut library = Library::default();
        Some(library.add(pack).map(|()| library))
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
            match object
                .script
                .filter(|_| object.script_file == 0)
                .and_then(|script| names.get(&script))
            {
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

    /// Takes the trees and samples of script classes from their layouts, and
    /// makes their `MonoScript`s. The execution order is that of a
    /// `MonoScript` read before, or zero.
    pub fn add_scripts(
        &mut self,
        layout: &super::scripts::Layout,
        execution_orders: &HashMap<String, i64>,
    ) -> Result<Vec<(String, String)>> {
        let mono_script = self
            .classes
            .get(&MONO_SCRIPT)
            .context("no type tree of MonoScript")?
            .sample
            .clone();
        let mut failed = Vec::new();
        for (name, script) in layout.scripts() {
            if script.generic {
                continue;
            }
            let (tree, sample) = match layout.script_kind(name) {
                Ok(kind) => kind,
                Err(error) => {
                    failed.push((name.clone(), format!("{error:#}")));
                    continue;
                }
            };
            let type_hash: Vec<u8> = tree.entry[23..39].to_vec();
            let mut object = self
                .script_objects
                .get(name)
                .cloned()
                .unwrap_or_else(|| mono_script.clone());
            for (field, value) in [
                ("m_Name", &script.name),
                ("m_ClassName", &script.name),
                ("m_Namespace", &script.namespace),
                ("m_AssemblyName", &script.assembly),
            ] {
                if let Some(target) = object.field_mut(field) {
                    *target = Val::Str(value.clone());
                }
            }
            if let Some(order) = object.field_mut("m_ExecutionOrder") {
                *order = Val::I(execution_orders.get(&script.name).copied().unwrap_or(0));
            }
            if let Some(Val::Map(bytes)) = object.field_mut("m_PropertiesHash") {
                for (index, (_, byte)) in bytes.iter_mut().enumerate() {
                    *byte = Val::U(type_hash[index] as u64);
                }
            }
            self.script_objects.insert(name.clone(), object);
            self.scripts.insert(
                name.clone(),
                Kind {
                    tree: Arc::new(tree),
                    sample,
                },
            );
        }
        for managed in layout.managed() {
            let key = (
                managed.name.clone(),
                managed.namespace.clone(),
                managed.assembly.clone(),
            );
            match layout.managed_kind(managed) {
                Ok((tree, sample)) => {
                    self.managed.insert(
                        key,
                        Kind {
                            tree: Arc::new(tree),
                            sample,
                        },
                    );
                }
                Err(error) => failed.push((
                    full_name(&managed.namespace, &managed.name),
                    format!("{error:#}"),
                )),
            }
        }
        Ok(failed)
    }

    /// The tree and sample of a class held by managed reference, by the
    /// class name, namespace and assembly an asset names it by.
    pub fn managed(&self, class: &str, namespace: &str, assembly: &str) -> Option<&Kind> {
        self.managed.get(&(
            class.to_string(),
            namespace.to_string(),
            assembly.to_string(),
        ))
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

    /// The script class of the assembly `assembly` that a reference names by
    /// `file_id` (a plugin's classes have no source of their own).
    pub fn assembly_script(&self, assembly: &str, file_id: i64) -> Option<String> {
        self.script_objects.iter().find_map(|(name, object)| {
            let text = |field: &str| text_of(object.field(field));
            (text("m_AssemblyName") == assembly
                && super::scripts::assembly_script_file_id(
                    &text("m_Namespace"),
                    &text("m_ClassName"),
                ) == file_id)
                .then(|| name.clone())
        })
    }
}

/// The execution order each script's `.cs.meta` sets (`MonoImporter`), by
/// the file's name, which is its class's, in the project's assets and
/// packages.
pub fn script_execution_orders(project: &Path) -> HashMap<String, i64> {
    let mut orders = HashMap::new();
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
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".cs.meta") else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let order = text
                .lines()
                .find_map(|line| line.trim().strip_prefix("executionOrder:"))
                .and_then(|value| value.trim().parse::<i64>().ok())
                .unwrap_or(0);
            if order != 0 {
                orders.insert(stem.to_string(), order);
            }
        }
    }
    orders
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

/// The former names of serialized script fields, from the
/// `[FormerlySerializedAs("old")]` attributes of a project's C# sources.
#[derive(Default)]
pub struct Renames {
    former: HashMap<String, Vec<String>>,
}

impl Renames {
    pub fn of_sources<'a>(texts: impl IntoIterator<Item = &'a str>) -> Self {
        let attribute = regex::Regex::new(
            r#"FormerlySerializedAs\(\s*"(\w+)"\s*\)\s*\][^;={}()]*?(\w+)\s*[;=]"#,
        )
        .expect("valid pattern");
        let mut former: HashMap<String, Vec<String>> = HashMap::new();
        for text in texts {
            for capture in attribute.captures_iter(text) {
                let list = former.entry(capture[2].to_string()).or_default();
                if !list.contains(&capture[1].to_string()) {
                    list.push(capture[1].to_string());
                }
            }
        }
        Renames { former }
    }

    fn former(&self, name: &str) -> &[String] {
        self.former.get(name).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// The fields of a document its conversion reads: those of its class's or
/// script's tree, under their names or former ones. `None` when the tree is
/// not known, and every field may be.
pub fn fields_read(
    library: &Library,
    renames: &Renames,
    document: &Document,
    script: Option<&str>,
) -> Option<HashSet<String>> {
    let kind = match script {
        Some(name) => library.script(name)?,
        None => library.class(document.class_id as i32)?,
    };
    let tree = &kind.tree;
    let mut names = HashSet::new();
    for child in tree.children(0) {
        let name = &tree.nodes[child].name;
        names.extend(renames.former(name).iter().cloned());
        names.insert(name.clone());
    }
    Some(names)
}

/// An object converted from its document.
pub struct Converted {
    pub tree: Arc<TypeTree>,
    pub value: Val,
    /// Fields the source lacks, taken from the engine's sample object.
    pub derived: Vec<String>,
    /// The classes the object holds by managed reference, by class name,
    /// namespace and assembly, in order of first use.
    pub held: Vec<(String, String, String)>,
}

/// Where a reference leads in the written files: the file index of the
/// pointer (0 for the same file) and the object's local ID there.
pub type Resolve<'a> = dyn FnMut(&Reference) -> Result<Option<(i32, i64)>> + 'a;

/// Converts one document of a source. `script` is the full class name of a
/// `MonoBehaviour`'s script.
pub fn convert(
    library: &Library,
    tags: &Tags,
    renames: &Renames,
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
        renames: if script.is_some() {
            Some(renames)
        } else {
            None
        },
        library,
        held: Vec::new(),
        nulls: false,
    };
    let source = Value::Object(document.data.clone());
    let mut value = walk.value(0, kind.sample.clone(), &source, "")?;
    let (derived, held) = (walk.derived, walk.held);
    settle(document, tags, &mut value)?;
    Ok(Converted {
        tree: kind.tree.clone(),
        value,
        derived,
        held,
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
    strip_unused_curves(value);
    // The Editor caches a renderer's sorting layer value; the Player looks
    // it up by the layer's ID and a build writes 0.
    if let Some(field) = value.field_mut("m_SortingLayer") {
        *field = Val::I(0);
    }
    Ok(())
}

/// A build keeps the keys of a min-max curve only where its mode reads
/// them: none for a constant (0) or two constants (3), the upper curve for a
/// curve (1), both for two curves (2).
fn strip_unused_curves(value: &mut Val) {
    match value {
        Val::Map(fields) => {
            let state = fields
                .iter()
                .find(|(k, _)| k == "minMaxState")
                .map(|(_, v)| match v {
                    Val::I(n) => *n,
                    Val::U(n) => *n as i64,
                    _ => -1,
                });
            let gradients = fields.iter().any(|(k, _)| k == "maxGradient");
            if let Some(state) = state {
                for (name, field) in fields.iter_mut() {
                    if gradients {
                        // A gradient the mode does not read is the default
                        // one: maxGradient serves a gradient (1), two
                        // gradients (3) and a random color (4).
                        let keep = match name.as_ref() {
                            "maxGradient" => matches!(state, 1 | 3 | 4),
                            "minGradient" => state == 3,
                            _ => true,
                        };
                        if !keep {
                            reset_gradient(field);
                        }
                        continue;
                    }
                    let keep = match name.as_ref() {
                        "maxCurve" => matches!(state, 1 | 2),
                        "minCurve" => state == 2,
                        _ => true,
                    };
                    // An unread curve is written as a new, empty one, in the
                    // default rotation order.
                    if !keep {
                        if let Some(Val::Seq(keys)) = field.field_mut("m_Curve") {
                            keys.clear();
                        }
                        if let Some(order) = field.field_mut("m_RotationOrder") {
                            *order = Val::I(4);
                        }
                    }
                }
            }
            for (_, field) in fields.iter_mut() {
                strip_unused_curves(field);
            }
        }
        Val::Seq(items) => items.iter_mut().for_each(strip_unused_curves),
        _ => {}
    }
}

/// Two white keys at the ends, as a new gradient has.
pub(super) fn reset_gradient(gradient: &mut Val) {
    let Val::Map(fields) = gradient else { return };
    for (name, field) in fields.iter_mut() {
        *field = match name.as_ref() {
            "key0" | "key1" => Val::map([
                ("r", Val::F(1.0)),
                ("g", Val::F(1.0)),
                ("b", Val::F(1.0)),
                ("a", Val::F(1.0)),
            ]),
            "ctime1" | "atime1" => Val::U(65535),
            "m_ColorSpace" => Val::I(-1),
            "m_NumColorKeys" | "m_NumAlphaKeys" => Val::U(2),
            _ => Val::Zero,
        };
    }
}

fn clear_pointers(value: &mut Val) {
    match value {
        Val::Map(fields) => {
            let pointer = fields.len() == 2
                && fields.iter().any(|(k, _)| k == "m_FileID")
                && fields.iter().any(|(k, _)| k == "m_PathID");
            for (_, field) in fields.iter_mut() {
                if pointer {
                    *field = Val::I(0);
                } else {
                    clear_pointers(field);
                }
            }
        }
        Val::Seq(items) => items.iter_mut().for_each(clear_pointers),
        _ => {}
    }
}

struct Walk<'t, 'r, 'a> {
    tree: &'t TypeTree,
    resolve: &'r mut Resolve<'a>,
    derived: Vec<String>,
    /// Former names of script fields, for a script's object.
    renames: Option<&'t Renames>,
    library: &'t Library,
    /// The classes held by managed reference, in order of first use.
    held: Vec<(String, String, String)>,
    /// Whether a field held by managed reference holds no object.
    nulls: bool,
}

/// The ID of a field held by managed reference that holds no object.
const NO_REFERENCE: i64 = -2;

/// Whether a value holds a field held by managed reference without an object.
fn holds_null_reference(value: &Val) -> bool {
    match value {
        Val::Map(fields) if fields.len() == 1 && fields[0].0 == "rid" => {
            fields[0].1 == Val::I(NO_REFERENCE)
        }
        Val::Map(fields) => fields.iter().any(|(_, field)| holds_null_reference(field)),
        Val::Seq(items) => items.iter().any(holds_null_reference),
        _ => false,
    }
}

impl Walk<'_, '_, '_> {
    /// A field the source lacks: the sample's value, but no reference into
    /// the sample's file.
    fn derive(&mut self, mut sample: Val, path: &str) -> Val {
        self.derived.push(path.to_string());
        clear_pointers(&mut sample);
        self.nulls |= holds_null_reference(&sample);
        sample
    }

    /// The registry of the objects held by managed reference, of format 2:
    /// each object by its ID (in a source of format 1, the hexadecimal key
    /// of its entry), its class and its fields laid out by the class's
    /// tree, in order of ID; with an empty first entry for "no object"
    /// when a field holds none.
    fn registry(&mut self, source: &Value, path: &str) -> Result<Val> {
        let mut entries: Vec<(i64, &Value, &Value)> = Vec::new();
        if let Value::Object(fields) = source {
            if let Some(Value::Array(ids)) = fields.get("RefIds") {
                for entry in ids {
                    let rid = entry
                        .get("rid")
                        .and_then(Value::as_str)
                        .and_then(|rid| rid.parse().ok())
                        .with_context(|| format!("{path}: an object without an ID"))?;
                    entries.push((
                        rid,
                        entry.get("type").unwrap_or(&Value::Null),
                        entry.get("data").unwrap_or(&Value::Null),
                    ));
                }
            } else {
                for (key, entry) in fields.iter().filter(|(key, _)| *key != "version") {
                    let rid = i64::from_str_radix(key, 16)
                        .with_context(|| format!("{path}: object key {key} is not an ID"))?;
                    entries.push((
                        rid,
                        entry.get("type").unwrap_or(&Value::Null),
                        entry.get("data").unwrap_or(&Value::Null),
                    ));
                }
            }
        }
        entries.sort_by_key(|(rid, ..)| *rid);
        let mut objects = Vec::with_capacity(entries.len() + 1);
        for (rid, kind, data) in entries {
            let part = |name: &str| {
                kind.get(name)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let (class, namespace, assembly) = (part("class"), part("ns"), part("asm"));
            let held = self.library.managed(&class, &namespace, &assembly).with_context(|| {
                format!("{path}: no layout of the class {namespace}.{class} ({assembly}) held by managed reference")
            })?;
            let object_path = format!("{path}[{rid}]");
            let mut walk = Walk {
                tree: &held.tree,
                resolve: &mut *self.resolve,
                derived: Vec::new(),
                renames: self.renames,
                library: self.library,
                held: Vec::new(),
                nulls: false,
            };
            let value = walk.value(0, held.sample.clone(), data, &object_path)?;
            self.derived.append(&mut walk.derived);
            self.nulls |= walk.nulls;
            for inner in walk.held {
                if !self.held.contains(&inner) {
                    self.held.push(inner);
                }
            }
            let key = (class.clone(), namespace.clone(), assembly.clone());
            if !self.held.contains(&key) {
                self.held.push(key);
            }
            objects.push(reference_object(rid, &class, &namespace, &assembly, value));
        }
        if self.nulls {
            objects.insert(0, reference_object(NO_REFERENCE, "", "", "", Val::Zero));
        }
        Ok(Val::Map(vec![
            (Cow::Borrowed("version"), Val::I(2)),
            (Cow::Borrowed("RefIds"), Val::Seq(objects)),
        ]))
    }

    fn value(&mut self, index: usize, sample: Val, source: &Value, path: &str) -> Result<Val> {
        let node = &self.tree.nodes[index];
        let children = self.tree.children(index);
        // A field held by managed reference: the ID of its object, `rid` in a
        // source of format 2 and `id` in one of format 1.
        if matches!(
            node.type_name.as_str(),
            "managedReference" | "managedRefArrayItem"
        ) {
            let rid = match source {
                Value::Object(fields) => fields
                    .get("rid")
                    .or_else(|| fields.get("id"))
                    .and_then(Value::as_str)
                    .and_then(|rid| rid.parse().ok()),
                _ => None,
            }
            .unwrap_or(NO_REFERENCE);
            self.nulls |= rid == NO_REFERENCE;
            return Ok(Val::Map(vec![(Cow::Borrowed("rid"), Val::I(rid))]));
        }
        if node.type_name == "ManagedReferencesRegistry" {
            return self.registry(source, path);
        }
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
            // A field read under a name it had before (`FormerlySerializedAs`).
            let authored = fields.get(&name).or_else(|| {
                self.renames?
                    .former(&name)
                    .iter()
                    .find_map(|former| fields.get(former))
            });
            let value = match authored {
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
                // A registry follows the fields whose objects it holds.
                None if self.tree.nodes[child].type_name == "ManagedReferencesRegistry"
                    && !(self.tree.class_id == -1 && index == 0) =>
                {
                    self.registry(&Value::Null, &child_path)?
                }
                None => self.derive(sample, &child_path),
            };
            values.push((Cow::Owned(name), value));
        }
        Ok(Val::Map(values))
    }
}

/// One entry of a managed reference registry.
fn reference_object(rid: i64, class: &str, namespace: &str, assembly: &str, data: Val) -> Val {
    Val::Map(vec![
        (Cow::Borrowed("rid"), Val::I(rid)),
        (
            Cow::Borrowed("type"),
            Val::Map(vec![
                (Cow::Borrowed("class"), Val::Str(class.to_string())),
                (Cow::Borrowed("ns"), Val::Str(namespace.to_string())),
                (Cow::Borrowed("asm"), Val::Str(assembly.to_string())),
            ]),
        ),
        (Cow::Borrowed("data"), data),
    ])
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

    /// The former names of the script fields of every C# source.
    pub fn renames(&self) -> Renames {
        let texts: Vec<String> = self
            .paths
            .values()
            .filter(|path| path.extension().is_some_and(|e| e == "cs"))
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .filter(|text| text.contains("FormerlySerializedAs"))
            .collect();
        Renames::of_sources(texts.iter().map(String::as_str))
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

    /// The GUID of the script file of a class (`Namespace.Class`): a `.cs`
    /// of the class's name that declares its namespace.
    pub fn script_guid(&self, class: &str) -> Option<String> {
        let short = class.rsplit('.').next()?;
        let mut found: Vec<&String> = self
            .paths
            .iter()
            .filter(|(_, path)| {
                path.extension().is_some_and(|e| e == "cs")
                    && path.file_stem().is_some_and(|stem| stem == short)
            })
            .map(|(guid, _)| guid)
            .filter(|guid| self.script_class(guid).as_deref() == Some(class))
            .collect();
        found.sort();
        found.first().map(|guid| guid.to_string())
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
        let object = convert(
            library,
            tags,
            &Renames::default(),
            document,
            script.as_deref(),
            &mut resolve,
        )
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
            script_file: 0,
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
            script_file: 0,
        });
    }
    let external_refs: Vec<super::External> = externals
        .iter()
        .map(|guid| super::External {
            guid: guid.clone(),
            path: String::new(),
        })
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
                script_file: 0,
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
            &Renames::default(),
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

#[cfg(test)]
mod rename_tests {
    use super::*;

    #[test]
    fn former_names_come_from_the_attributes_of_fields() {
        let renames = Renames::of_sources([
            "[SerializeField] [FormerlySerializedAs(\"m_enableWordWrapping\")]\n protected TextWrappingModes m_TextWrappingMode;",
            "[FormerlySerializedAs(\"speed\")] public float velocity = 2f;",
        ]);
        assert_eq!(
            renames.former("m_TextWrappingMode"),
            ["m_enableWordWrapping"]
        );
        assert_eq!(renames.former("velocity"), ["speed"]);
        assert!(renames.former("other").is_empty());
    }
}
