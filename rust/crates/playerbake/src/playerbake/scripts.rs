//! Player type trees of script classes, laid out from what `tools/ScriptLayouts`
//! reads of the compiled assemblies: the serialized fields of each class, and of
//! each `[Serializable]` type they hold, by Unity's serialization rules. What the
//! engine defines natively (the header every script object starts with, the
//! layout of `Vector3`, `AnimationCurve` and the like) comes from templates of the
//! release, taken from type trees the engine wrote.
use super::typetree::{Node, TypeTree, Val};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

/// What `ScriptLayouts` prints.
#[derive(Deserialize, Default)]
pub struct Layouts {
    pub scripts: BTreeMap<String, Script>,
    pub types: BTreeMap<String, Composite>,
    /// The classes fields held by managed reference can hold.
    #[serde(default)]
    pub managed: Vec<Managed>,
}

/// A class an object holds by managed reference, as the asset names it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Managed {
    pub assembly: String,
    pub namespace: String,
    /// `Outer/Inner` for a nested class.
    pub name: String,
    /// The name its type tree gives it.
    pub tree_name: String,
    pub fields: Vec<Field>,
}

#[derive(Deserialize)]
pub struct Script {
    pub assembly: String,
    pub namespace: String,
    pub name: String,
    #[serde(rename = "abstract")]
    pub is_abstract: bool,
    pub generic: bool,
    pub fields: Vec<Field>,
}

#[derive(Deserialize)]
pub struct Composite {
    pub name: String,
    #[serde(rename = "struct")]
    pub is_struct: bool,
    pub fields: Vec<Field>,
}

#[derive(Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    #[serde(default)]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub formerly: Option<Vec<String>>,
    /// What the class's constructor assigns the field, as far as its IL shows.
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct Kind {
    pub of: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub element: Option<Box<Kind>>,
    #[serde(default)]
    pub list: Option<bool>,
}

/// Meta flags of type tree nodes.
const HIDDEN: u32 = 0x1;
const BOOLEAN: u32 = 0x100;
const CHARACTER: u32 = 0x200_0000;
const ALIGN: u32 = 0x4000;
const CHILD_ALIGNS: u32 = 0x8000;

/// The engine value types a script may hold, by their C# name and the name
/// their type tree gives them, with the byte size that tells two of the same
/// tree name apart.
const ENGINE_VALUES: &[(&str, &str, i32)] = &[
    ("Vector2", "Vector2f", 8),
    ("Vector3", "Vector3f", 12),
    ("Vector4", "Vector4f", 16),
    ("Quaternion", "Quaternionf", 16),
    ("Matrix4x4", "Matrix4x4f", 64),
    ("Color", "ColorRGBA", 16),
    ("Color32", "ColorRGBA", 4),
    ("Rect", "Rectf", 16),
    ("LayerMask", "BitField", 4),
    ("AnimationCurve", "AnimationCurve", -1),
    ("Gradient", "Gradient", -2),
    ("RectOffset", "RectOffset", -2),
    ("GUIStyle", "GUIStyle", -2),
    ("Bounds", "AABB", 24),
    ("Vector2Int", "int2_storage", 8),
    ("Vector3Int", "int3_storage", 12),
    ("RectInt", "RectInt", 16),
    ("BoundsInt", "BoundsInt", 24),
    ("Hash128", "Hash128", 16),
    ("SphericalHarmonicsL2", "SphericalHarmonicsL2", 108),
    ("PropertyName", "PropertyName", -2),
];

/// Subtrees the engine defines, their levels starting at zero.
#[derive(Default)]
pub struct Templates {
    /// The fields every script object starts with (`m_GameObject` to `m_Name`).
    pub header: Vec<Node>,
    /// The subtree of a script field of an engine value type, by C# name.
    pub values: HashMap<String, Vec<Node>>,
    /// The registry of objects held by managed reference, at level one.
    pub registry: Vec<Node>,
}

fn subtree(nodes: &[Node], index: usize) -> Vec<Node> {
    let level = nodes[index].level;
    let mut end = index + 1;
    while end < nodes.len() && nodes[end].level > level {
        end += 1;
    }
    nodes[index..end]
        .iter()
        .map(|node| Node {
            level: node.level - level,
            ..node.clone()
        })
        .collect()
}

/// Meta flags an engine class's field passes down to the value it holds:
/// hidden, not editable and not animated.
const CONTEXT: u32 = HIDDEN | 0x10 | 0x80_0000;

/// The name a pack gives the registry of objects held by managed reference.
const REGISTRY: &str = "references";

impl Templates {
    /// Takes the templates from type trees the engine wrote: the header of the
    /// first script in the Player's format, and each engine value type where a script field holds one
    /// that is not hidden. A value type no script holds is taken from the
    /// classes of the engine that hold it, without the flags their fields pass
    /// down (hidden, not editable, not animated, aligned after), in the shape
    /// most of them give it (`AnimationCurve` of floats, not of vectors).
    pub fn of_trees<'a>(trees: impl IntoIterator<Item = &'a TypeTree>) -> Self {
        let mut templates = Templates::default();
        let mut engine: HashMap<&str, HashMap<Vec<Node>, usize>> = HashMap::new();
        for tree in trees {
            let nodes = &tree.nodes;
            let script = tree.class_id == super::typetree::SCRIPT_CLASS;
            // The Editor's own files hold its trees, whose header starts with
            // `m_ObjectHideFlags`; a Player's starts with `m_GameObject`.
            let player = nodes.get(1).is_some_and(|node| node.name == "m_GameObject");
            if script && player && templates.header.is_empty() {
                let mut index = 1;
                while index < nodes.len() {
                    let field = subtree(nodes, index);
                    let last = nodes[index].name == "m_Name";
                    index += field.len();
                    templates.header.extend(field.into_iter().map(|node| Node {
                        level: node.level + 1,
                        ..node
                    }));
                    if last {
                        break;
                    }
                }
            }
            for (index, node) in nodes.iter().enumerate().skip(1) {
                if script
                    && node.level == 1
                    && node.type_name == "ManagedReferencesRegistry"
                    && templates.registry.is_empty()
                {
                    templates.registry = subtree(nodes, index)
                        .into_iter()
                        .map(|node| Node {
                            level: node.level + 1,
                            ..node
                        })
                        .collect();
                }
                if script && node.meta & HIDDEN != 0 {
                    continue;
                }
                for (csharp, tree_name, size) in ENGINE_VALUES {
                    if node.type_name != *tree_name || (*size >= -1 && node.size != *size) {
                        continue;
                    }
                    // A template is named after its C# type; a field holding
                    // one takes the field's name.
                    let mut value = subtree(nodes, index);
                    value[0].name = (*csharp).to_string();
                    if script {
                        templates
                            .values
                            .entry((*csharp).to_string())
                            .or_insert(value);
                    } else {
                        for (at, node) in value.iter_mut().enumerate() {
                            node.meta &= !CONTEXT;
                            if at == 0 {
                                node.meta &= !ALIGN;
                            }
                            node.align = node.meta & ALIGN != 0;
                        }
                        *engine.entry(csharp).or_default().entry(value).or_default() += 1;
                    }
                }
            }
        }
        for (csharp, shapes) in engine {
            if templates.values.contains_key(csharp) {
                continue;
            }
            let mut shapes: Vec<_> = shapes.into_iter().collect();
            shapes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.len().cmp(&b.0.len())));
            templates
                .values
                .insert(csharp.to_string(), shapes.swap_remove(0).0);
        }
        templates
    }

    /// The templates of a release the tool ships, written by `to_pack`.
    pub fn of_release(unity_version: &str) -> Option<Self> {
        let entry: &'static [u8] = match unity_version {
            "6000.0.71f1" => include_bytes!("engine/6000.0.71f1/script-templates.bin"),
            _ => return None,
        };
        let tree = TypeTree::parse(entry).expect("a shipped pack of templates parses");
        Some(Self::of_pack(&tree))
    }

    /// Reads templates back from the tree `to_pack` made.
    pub fn of_pack(tree: &TypeTree) -> Self {
        let mut templates = Templates::default();
        let nodes = &tree.nodes;
        let mut index = 1;
        let mut header = true;
        while index < nodes.len() {
            let field = subtree(nodes, index);
            index += field.len();
            if header {
                header = field[0].name != "m_Name";
                templates.header.extend(field.into_iter().map(|node| Node {
                    level: node.level + 1,
                    ..node
                }));
            } else if field[0].name == REGISTRY {
                templates.registry = field
                    .into_iter()
                    .map(|node| Node {
                        level: node.level + 1,
                        ..node
                    })
                    .collect();
            } else {
                templates.values.insert(field[0].name.clone(), field);
            }
        }
        templates
    }

    /// A script type tree that holds every template, the values as fields
    /// named after their C# types, for a release's pack.
    pub fn to_pack(&self) -> Result<TypeTree> {
        let mut nodes = vec![node("MonoBehaviour", "Base", 0, -1, CHILD_ALIGNS)];
        nodes.extend(self.header.iter().cloned());
        for value in self.values.iter().collect::<BTreeMap<_, _>>().values() {
            nodes.extend(value.iter().map(|template| Node {
                level: template.level + 1,
                ..template.clone()
            }));
        }
        if let Some((first, rest)) = self.registry.split_first() {
            nodes.push(Node {
                name: REGISTRY.to_string(),
                ..first.clone()
            });
            nodes.extend(rest.iter().cloned());
        }
        TypeTree::of_nodes(super::typetree::SCRIPT_CLASS, Some([0; 16]), [0; 16], nodes)
    }
}

/// Lays out script classes and their serializable types.
pub struct Layout<'a> {
    layouts: &'a Layouts,
    templates: &'a Templates,
}

/// How deep `[Serializable]` types nest before Unity stops serializing them.
const DEPTH_LIMIT: usize = 10;

impl<'a> Layout<'a> {
    pub fn new(layouts: &'a Layouts, templates: &'a Templates) -> Self {
        Self { layouts, templates }
    }

    pub fn scripts(&self) -> impl Iterator<Item = (&'a String, &'a Script)> {
        self.layouts.scripts.iter()
    }

    /// The nodes of the type tree of the script class `name`
    /// (`Namespace.Class`, or `Class` without a namespace).
    pub fn script(&self, name: &str) -> Result<Vec<Node>> {
        let Some(script) = self.layouts.scripts.get(name) else {
            bail!("no script class {name}");
        };
        if self.templates.header.is_empty() {
            bail!("no script header template");
        }
        let mut nodes = vec![node("MonoBehaviour", "Base", 0, -1, 0)];
        nodes.extend(self.templates.header.iter().cloned());
        for field in &script.fields {
            self.field(field, 1, 0, &mut nodes)
                .with_context(|| format!("field {} of {name}", field.name))?;
        }
        // Objects held by managed reference follow the fields.
        if nodes.iter().any(|node| node.type_flags & 2 != 0) {
            if self.templates.registry.is_empty() {
                bail!("no template of the managed reference registry");
            }
            nodes.extend(self.templates.registry.iter().cloned());
        }
        settle(&mut nodes);
        Ok(nodes)
    }

    /// The type tree of a script class and an object of it as its constructor
    /// leaves it, for the fields an asset lacks.
    pub fn script_kind(&self, name: &str) -> Result<(TypeTree, Val)> {
        let script = &self.layouts.scripts[name];
        let nodes = self.script(name)?;
        let script_hash = script_hash(&script.name, &script.namespace, &script.assembly);
        let type_hash = properties_hash(&script.name, &script.namespace, &script.assembly, &nodes);
        let tree = TypeTree::of_nodes(
            super::typetree::SCRIPT_CLASS,
            Some(script_hash),
            type_hash,
            nodes,
        )?;
        let mut sample = zero(&tree, 0);
        for field in &script.fields {
            if let Some(target) = sample.field_mut(&field.name) {
                self.apply(&field.kind, field.default.as_ref(), target);
            }
        }
        if let Some(Val::Map(fields)) = sample.field_mut("references") {
            for (name, value) in fields {
                if name == "version" {
                    *value = Val::I(2);
                }
            }
        }
        Ok((tree, sample))
    }

    /// The type tree of a class held by managed reference, as a file's table
    /// of such types holds it, and an object of it as its constructor leaves
    /// it. A class that holds objects by managed reference itself ends in a
    /// registry, which its objects leave empty.
    pub fn managed_kind(&self, managed: &Managed) -> Result<(TypeTree, Val)> {
        let mut nodes = vec![node(&managed.tree_name, "Base", 0, -1, 0)];
        for field in &managed.fields {
            self.field(field, 1, 0, &mut nodes).with_context(|| {
                format!(
                    "field {} of {}.{}",
                    field.name, managed.namespace, managed.name
                )
            })?;
        }
        if nodes.iter().any(|node| node.type_flags & 2 != 0) {
            if self.templates.registry.is_empty() {
                bail!("no template of the managed reference registry");
            }
            nodes.extend(self.templates.registry.iter().cloned());
        }
        settle(&mut nodes);
        let tree = TypeTree::of_nodes(-1, None, [0; 16], nodes)?;
        let mut sample = zero(&tree, 0);
        for field in &managed.fields {
            if let Some(target) = sample.field_mut(&field.name) {
                self.apply(&field.kind, field.default.as_ref(), target);
            }
        }
        Ok((tree, sample))
    }

    pub fn managed(&self) -> &'a [Managed] {
        &self.layouts.managed
    }

    /// Sets what a constructor assigns, over a value of zeros.
    fn apply(&self, kind: &Kind, default: Option<&serde_json::Value>, target: &mut Val) {
        let named = kind.name.as_deref().unwrap_or_default();
        match kind.of.as_str() {
            "primitive" => {
                if let Some(number) = default.and_then(serde_json::Value::as_f64) {
                    *target = primitive(named, number);
                }
            }
            // The elements a collection initializer adds, of values only.
            "array" => {
                let (Some(items), Some(element)) =
                    (default.and_then(serde_json::Value::as_array), &kind.element)
                else {
                    return;
                };
                let element_name = element.name.as_deref().unwrap_or_default();
                let values: Option<Vec<Val>> = items
                    .iter()
                    .map(|item| match element.of.as_str() {
                        "primitive" => item.as_f64().map(|n| primitive(element_name, n)),
                        "string" => item.as_str().map(|text| Val::Str(text.to_string())),
                        _ => None,
                    })
                    .collect();
                if let Some(values) = values {
                    *target = Val::Seq(values);
                }
            }
            "string" => {
                if let Some(text) = default.and_then(serde_json::Value::as_str) {
                    *target = Val::Str(text.to_string());
                }
            }
            "engine" => {
                // A curve or gradient is made by its constructor even where
                // the script's leaves it null.
                match named {
                    "AnimationCurve" => {
                        for (field, value) in [
                            ("m_PreInfinity", 2),
                            ("m_PostInfinity", 2),
                            ("m_RotationOrder", 4),
                        ] {
                            if let Some(target) = target.field_mut(field) {
                                *target = Val::I(value);
                            }
                        }
                    }
                    "Gradient" => super::authored::reset_gradient(target),
                    _ => {}
                }
                let Some(values) = default.and_then(serde_json::Value::as_object) else {
                    return;
                };
                let number = |name: &str| values.get(name).and_then(|v| v.as_f64()).unwrap_or(0.0);
                if named == "Color32" {
                    let packed = ["r", "g", "b", "a"]
                        .iter()
                        .enumerate()
                        .map(|(index, name)| (number(name) as u64 & 0xff) << (8 * index))
                        .sum();
                    if let Some(field) = target.field_mut("rgba") {
                        *field = Val::U(packed);
                    }
                    return;
                }
                for (name, value) in values {
                    if let (Some(field), Some(value)) = (target.field_mut(name), value.as_f64()) {
                        *field = Val::F(value as f32);
                    }
                }
            }
            "composite" => {
                let Some(composite) = self.layouts.types.get(named) else {
                    return;
                };
                for field in &composite.fields {
                    let value = default
                        .and_then(|values| values.get(&field.name))
                        .or(field.default.as_ref());
                    if let Some(target) = target.field_mut(&field.name) {
                        self.apply(&field.kind, value, target);
                    }
                }
            }
            "reference" => {
                if let Some(rid) = target.field_mut("rid") {
                    // No object.
                    *rid = Val::I(-2);
                }
            }
            _ => {}
        }
    }

    fn field(&self, field: &Field, level: u8, depth: usize, nodes: &mut Vec<Node>) -> Result<()> {
        let start = nodes.len();
        if !self.kind(&field.kind, &field.name, level, depth, nodes)? {
            return Ok(());
        }
        if field.hidden == Some(true) {
            for node in &mut nodes[start..] {
                node.meta |= HIDDEN;
            }
        }
        Ok(())
    }

    /// Appends the subtree of a value of `kind`; false when Unity does not
    /// serialize it at this depth.
    fn kind(
        &self,
        kind: &Kind,
        name: &str,
        level: u8,
        depth: usize,
        nodes: &mut Vec<Node>,
    ) -> Result<bool> {
        let named = kind.name.as_deref().unwrap_or_default();
        match kind.of.as_str() {
            "primitive" => {
                let (type_name, size, meta) = match named {
                    "bool" => ("UInt8", 1, ALIGN | BOOLEAN),
                    "byte" => ("UInt8", 1, ALIGN),
                    "sbyte" => ("SInt8", 1, ALIGN),
                    "char" => ("UInt16", 2, ALIGN | CHARACTER),
                    "short" => ("SInt16", 2, ALIGN),
                    "ushort" => ("UInt16", 2, ALIGN),
                    "int" => ("int", 4, 0),
                    "uint" => ("unsigned int", 4, 0),
                    "long" => ("SInt64", 8, 0),
                    "ulong" => ("UInt64", 8, 0),
                    "float" => ("float", 4, 0),
                    "double" => ("double", 8, 0),
                    other => bail!("unknown primitive {other}"),
                };
                nodes.push(node(type_name, name, level, size, meta));
            }
            "string" => {
                nodes.push(node("string", name, level, -1, 0));
                let mut array = node("Array", "Array", level + 1, -1, ALIGN | HIDDEN);
                array.array = true;
                array.type_flags = 1;
                nodes.push(array);
                nodes.push(node("int", "size", level + 2, 4, HIDDEN));
                nodes.push(node("char", "data", level + 2, 1, HIDDEN));
            }
            "object" => {
                nodes.push(node(&format!("PPtr<${named}>"), name, level, 12, 0));
                nodes.push(node("int", "m_FileID", level + 1, 4, 0x80_0001));
                nodes.push(node("SInt64", "m_PathID", level + 1, 8, 0x80_0001));
            }
            "engine" => {
                let Some(template) = self.templates.values.get(named) else {
                    bail!("no template of the engine type {named}");
                };
                for (index, template) in template.iter().enumerate() {
                    let mut copy = template.clone();
                    copy.level += level;
                    if index == 0 {
                        copy.name = name.to_string();
                    }
                    nodes.push(copy);
                }
            }
            "array" => {
                let Some(element) = &kind.element else {
                    bail!("array without element");
                };
                let at = nodes.len();
                nodes.push(node("vector", name, level, -1, 0));
                let mut array = node("Array", "Array", level + 1, -1, 0);
                array.array = true;
                array.type_flags = 1;
                nodes.push(array);
                nodes.push(node("int", "size", level + 2, 4, 0));
                if !self.kind(element, "data", level + 2, depth, nodes)? {
                    nodes.truncate(at);
                    return Ok(false);
                }
                match element.of.as_str() {
                    // An array of a serializable type takes the name of that type.
                    "composite" => nodes[at].type_name = nodes[at + 3].type_name.clone(),
                    // One of managed references the name of the referenced type.
                    "reference" => {
                        nodes[at].type_name = element.name.clone().unwrap_or_default();
                        nodes[at].type_flags = 8;
                        nodes[at + 3].type_name = "managedRefArrayItem".into();
                    }
                    // An array of values aligns after its elements; elements
                    // narrower than four bytes do not align each, and their
                    // flags hold for the whole array.
                    "primitive" | "string" | "engine" => {
                        let data = nodes[at + 3].meta;
                        if nodes[at + 3].size > 0 && nodes[at + 3].size < 4 {
                            let flags = data & !ALIGN;
                            nodes[at + 3].meta = flags;
                            nodes[at + 2].meta |= flags;
                            nodes[at + 1].meta |= flags;
                            nodes[at].meta |= flags | ALIGN;
                        }
                        nodes[at + 1].meta |= ALIGN;
                    }
                    _ => {}
                }
            }
            "composite" => {
                if depth >= DEPTH_LIMIT {
                    return Ok(false);
                }
                let Some(composite) = self.layouts.types.get(named) else {
                    bail!("no serializable type {named}");
                };
                nodes.push(node(&composite.name, name, level, -1, 0));
                for field in &composite.fields {
                    self.field(field, level + 1, depth + 1, nodes)?;
                }
            }
            "reference" => {
                let mut reference = node("managedReference", name, level, 8, 0);
                reference.type_flags = 2;
                nodes.push(reference);
                nodes.push(node("SInt64", "rid", level + 1, 8, 0));
            }
            other => bail!("unknown kind {other}"),
        }
        Ok(true)
    }
}

fn primitive(name: &str, number: f64) -> Val {
    match name {
        "float" => Val::F(number as f32),
        "double" => Val::D(number),
        "bool" => Val::Bool(number != 0.0),
        "ulong" | "uint" | "ushort" | "byte" | "char" => Val::U(number as u64),
        _ => Val::I(number as i64),
    }
}

/// A value of zeros, empty strings and empty arrays.
fn zero(tree: &TypeTree, index: usize) -> Val {
    let node = &tree.nodes[index];
    if node.type_name == "string" {
        return Val::Str(String::new());
    }
    if tree.element(index).is_some() {
        return Val::Seq(Vec::new());
    }
    let children = tree.children(index);
    if children.is_empty() {
        return Val::Zero;
    }
    Val::Map(
        children
            .into_iter()
            .map(|child| {
                (
                    Cow::Owned(tree.nodes[child].name.clone()),
                    zero(tree, child),
                )
            })
            .collect(),
    )
}

/// The hash of a script in a type table: MD4 of its class name, namespace
/// and assembly name.
pub fn script_hash(class: &str, namespace: &str, assembly: &str) -> [u8; 16] {
    md4(format!("{class}{namespace}{assembly}").as_bytes())
}

/// The hash of a script's layout, which a type table states and its
/// MonoScript keeps as `m_PropertiesHash`: MD4 of the class name, namespace
/// and assembly name followed by each node of the type tree in order, its
/// type, name, size, type flags, version and alignment as four-byte integers.
pub fn properties_hash(class: &str, namespace: &str, assembly: &str, nodes: &[Node]) -> [u8; 16] {
    let mut message = format!("{class}{namespace}{assembly}").into_bytes();
    for node in nodes {
        message.extend_from_slice(node.type_name.as_bytes());
        message.extend_from_slice(node.name.as_bytes());
        message.extend_from_slice(&node.size.to_le_bytes());
        message.extend_from_slice(&u32::from(node.type_flags).to_le_bytes());
        message.extend_from_slice(&u32::from(node.version).to_le_bytes());
        message.extend_from_slice(&(node.meta & ALIGN).to_le_bytes());
    }
    md4(&message)
}

/// The file ID a reference gives a script class of a compiled assembly (a
/// plugin DLL): the first four bytes of MD4 of `s\0\0\0`, its namespace and
/// its name, as a little-endian integer.
pub fn assembly_script_file_id(namespace: &str, class: &str) -> i64 {
    let mut message = b"s\0\0\0".to_vec();
    message.extend_from_slice(namespace.as_bytes());
    message.extend_from_slice(class.as_bytes());
    let hash = md4(&message);
    i32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]) as i64
}

/// One round of MD4: its function, constant, word order and shifts.
type Round = (fn(u32, u32, u32) -> u32, u32, [usize; 16], [u32; 4]);

/// MD4 (RFC 1320).
fn md4(message: &[u8]) -> [u8; 16] {
    let mut data = message.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&((message.len() as u64) * 8).to_le_bytes());
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in data.chunks(64) {
        let x: Vec<u32> = block
            .chunks(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        let [mut a, mut b, mut c, mut d] = state;
        let rounds: [Round; 3] = [
            (
                |x, y, z| (x & y) | (!x & z),
                0,
                [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
                [3, 7, 11, 19],
            ),
            (
                |x, y, z| (x & y) | (x & z) | (y & z),
                0x5a82_7999,
                [0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15],
                [3, 5, 9, 13],
            ),
            (
                |x, y, z| x ^ y ^ z,
                0x6ed9_eba1,
                [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15],
                [3, 9, 11, 15],
            ),
        ];
        for (f, constant, order, shifts) in rounds {
            for (step, &index) in order.iter().enumerate() {
                let value = a
                    .wrapping_add(f(b, c, d))
                    .wrapping_add(x[index])
                    .wrapping_add(constant)
                    .rotate_left(shifts[step % 4]);
                (a, b, c, d) = (d, value, b, c);
            }
        }
        for (word, add) in state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(add);
        }
    }
    let mut out = [0; 16];
    for (chunk, word) in out.chunks_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    out
}

fn node(type_name: &str, name: &str, level: u8, size: i32, meta: u32) -> Node {
    Node {
        type_name: type_name.to_string(),
        name: name.to_string(),
        level,
        array: false,
        align: meta & ALIGN != 0,
        size,
        version: 1,
        type_flags: 0,
        meta,
    }
}

/// Marks each node whose descendants align and gives a structure the size
/// of its fields when all of them have one.
fn settle(nodes: &mut [Node]) {
    for index in (0..nodes.len()).rev() {
        let level = nodes[index].level;
        let mut end = index + 1;
        let mut aligns = false;
        let mut size = 0i32;
        let mut fixed = true;
        while end < nodes.len() && nodes[end].level > level {
            if nodes[end].meta & (ALIGN | CHILD_ALIGNS) != 0 {
                aligns = true;
            }
            if nodes[end].level == level + 1 {
                match nodes[end].size {
                    size_of if size_of >= 0 => size += size_of,
                    _ => fixed = false,
                }
            }
            end += 1;
        }
        if aligns {
            nodes[index].meta |= CHILD_ALIGNS;
        }
        let node = &mut nodes[index];
        if end > index + 1 && node.size < 0 && fixed && !node.array && node.type_name != "string" {
            node.size = size;
        }
        node.align = node.meta & ALIGN != 0;
    }
}

/// The first difference of two node lists, as text.
pub fn first_difference(ours: &[Node], theirs: &[Node]) -> Option<String> {
    let show = |node: &Node| {
        format!(
            "{}{} {} size={} v={} flags={} meta={:#x}",
            "  ".repeat(node.level as usize),
            node.type_name,
            node.name,
            node.size,
            node.version,
            node.type_flags,
            node.meta
        )
    };
    for index in 0..ours.len().max(theirs.len()) {
        let (a, b) = (ours.get(index), theirs.get(index));
        let same = match (a, b) {
            (Some(a), Some(b)) => show(a) == show(b),
            _ => false,
        };
        if !same {
            let path = |nodes: &[Node]| {
                let mut path = Vec::new();
                let mut level = u8::MAX;
                for node in nodes[..=index.min(nodes.len().saturating_sub(1))]
                    .iter()
                    .rev()
                {
                    if node.level < level {
                        path.push(node.name.clone());
                        level = node.level;
                    }
                }
                path.reverse();
                path.join(".")
            };
            return Some(format!(
                "at {} ({}):\n  ours   {}\n  theirs {}",
                index,
                path(if b.is_some() { theirs } else { ours }),
                a.map(show).unwrap_or_default(),
                b.map(show).unwrap_or_default()
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(nodes: &[Node]) -> Vec<String> {
        nodes
            .iter()
            .map(|node| {
                format!(
                    "{}{} {} {} {} {:#x}",
                    "  ".repeat(node.level as usize),
                    node.type_name,
                    node.name,
                    node.size,
                    node.type_flags,
                    node.meta
                )
            })
            .collect()
    }

    #[test]
    fn hashes_scripts_as_the_engine() {
        assert_eq!(
            md4(b"abc"),
            *b"\xa4\x48\x01\x7a\xaf\x21\xd8\x52\x5f\xc1\x0a\xe8\x7a\xa6\x72\x9d"
        );
        // UnityEngine.UI.Image in a file the worker wrote.
        let hash = script_hash("Image", "UnityEngine.UI", "UnityEngine.UI");
        assert_eq!(
            hash.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "76750b3b885cd805542d4471b739f45c"
        );
    }

    #[test]
    fn names_the_scripts_of_an_assembly_as_the_engine() {
        // Gadsme.dll's classes, as Bouncemasters' prefabs refer to them.
        assert_eq!(
            assembly_script_file_id("Gadsme", "GadsmePlacementBanner"),
            -2040588403
        );
        assert_eq!(
            assembly_script_file_id("Gadsme", "GadsmeMeshSerializer"),
            -126867728
        );
    }

    #[test]
    fn hashes_layouts_as_the_engine() {
        // SocialManager of Bouncemasters, which has no fields of its own.
        let mut nodes = vec![
            node("MonoBehaviour", "Base", 0, -1, 0x8000),
            node("PPtr<GameObject>", "m_GameObject", 1, 12, 0x41),
            node("int", "m_FileID", 2, 4, 0x41),
            node("SInt64", "m_PathID", 2, 8, 0x41),
            node("UInt8", "m_Enabled", 1, 1, 0x4101),
            node("PPtr<MonoScript>", "m_Script", 1, 12, 0),
            node("int", "m_FileID", 2, 4, 0x80_0001),
            node("SInt64", "m_PathID", 2, 8, 0x80_0001),
            node("string", "m_Name", 1, -1, 0x8_8001),
            node("Array", "Array", 2, -1, 0x8_4001),
            node("int", "size", 3, 4, 0x8_0001),
            node("char", "data", 3, 1, 0x8_0001),
        ];
        nodes[9].type_flags = 1;
        let hash = properties_hash("SocialManager", "", "Sportmasters", &nodes);
        assert_eq!(
            hash.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "6cd18acc0b48958b0fb76543ed33208d"
        );
    }

    #[test]
    fn takes_engine_values_from_engine_classes_without_their_context() {
        // Transform's rotation, and a hidden one of another engine class.
        let quaternion = |meta: u32| {
            let mut nodes = vec![node("Quaternionf", "m_LocalRotation", 1, 16, meta)];
            for axis in ["x", "y", "z", "w"] {
                nodes.push(node("float", axis, 2, 4, meta & !ALIGN));
            }
            nodes
        };
        let tree = |class_id, value: Vec<Node>| {
            let mut nodes = vec![node("Transform", "Base", 0, -1, 0)];
            nodes.extend(value);
            TypeTree::of_nodes(class_id, None, [0; 16], nodes).unwrap()
        };
        let trees = [
            tree(4, quaternion(0x20_4000)),
            tree(4, quaternion(0x20_0000)),
            tree(198, quaternion(0xa0_0001)),
        ];
        let templates = Templates::of_trees(&trees);
        assert_eq!(
            show(&templates.values["Quaternion"]),
            [
                "Quaternionf Quaternion 16 0 0x200000",
                "  float x 4 0 0x200000",
                "  float y 4 0 0x200000",
                "  float z 4 0 0x200000",
                "  float w 4 0 0x200000",
            ]
        );
    }

    #[test]
    fn reads_the_pack_of_a_release() {
        let templates = Templates::of_release("6000.0.71f1").unwrap();
        let back =
            Templates::of_pack(&TypeTree::parse(templates.to_pack().unwrap().entry).unwrap());
        assert_eq!(back.values, templates.values);
        assert_eq!(templates.header.len(), 11);
        assert_eq!(templates.header[0].name, "m_GameObject");
        for value in [
            "Vector3",
            "Quaternion",
            "Matrix4x4",
            "GUIStyle",
            "AnimationCurve",
        ] {
            assert!(templates.values.contains_key(value), "{value}");
        }
    }

    /// Layouts as the engine wrote them for Bouncemasters' scripts (6000.0.71f1).
    #[test]
    fn lays_out_fields_as_the_engine() {
        let layouts: Layouts = serde_json::from_value(serde_json::json!({
            "scripts": {"Sample": {"assembly": "A", "namespace": "", "name": "Sample",
                "abstract": false, "generic": false, "fields": [
                {"name": "flags", "type": {"of": "array", "list": true, "element": {"of": "primitive", "name": "bool"}}},
                {"name": "names", "type": {"of": "array", "element": {"of": "string"}}},
                {"name": "infos", "type": {"of": "array", "element": {"of": "composite", "name": "Info"}}},
                {"name": "target", "type": {"of": "object", "name": "Transform"}, "hidden": true}
            ]}},
            "types": {"Info": {"name": "Info", "struct": false, "fields": [
                {"name": "on", "type": {"of": "primitive", "name": "bool"}},
                {"name": "count", "type": {"of": "primitive", "name": "int"}}
            ]}}
        }))
        .unwrap();
        let templates = Templates {
            header: vec![node("int", "m_Header", 1, 4, 0)],
            ..Templates::default()
        };
        let nodes = Layout::new(&layouts, &templates).script("Sample").unwrap();
        assert_eq!(
            show(&nodes),
            [
                "MonoBehaviour Base -1 0 0x8000",
                "  int m_Header 4 0 0x0",
                "  vector flags -1 0 0xc100",
                "    Array Array -1 1 0x4100",
                "      int size 4 0 0x100",
                "      UInt8 data 1 0 0x100",
                "  vector names -1 0 0x8000",
                "    Array Array -1 1 0xc000",
                "      int size 4 0 0x0",
                "      string data -1 0 0x8000",
                "        Array Array -1 1 0x4001",
                "          int size 4 0 0x1",
                "          char data 1 0 0x1",
                "  Info infos -1 0 0x8000",
                "    Array Array -1 1 0x8000",
                "      int size 4 0 0x0",
                // The engine sums the sizes without padding (TMP_Character: 13).
                "      Info data 5 0 0x8000",
                "        UInt8 on 1 0 0x4100",
                "        int count 4 0 0x0",
                "  PPtr<$Transform> target 12 0 0x1",
                "    int m_FileID 4 0 0x800001",
                "    SInt64 m_PathID 8 0 0x800001",
            ]
        );
        let tree = TypeTree::of_nodes(114, Some([1; 16]), [2; 16], nodes).unwrap();
        assert_eq!(show(&tree.nodes)[2], "  vector flags -1 0 0xc100");
    }
}
