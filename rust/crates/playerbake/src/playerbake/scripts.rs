//! Player type trees of script classes, laid out from what `tools/ScriptLayouts`
//! reads of the compiled assemblies: the serialized fields of each class, and of
//! each `[Serializable]` type they hold, by Unity's serialization rules. What the
//! engine defines natively (the header every script object starts with, the
//! layout of `Vector3`, `AnimationCurve` and the like) comes from templates of the
//! release, taken from type trees the engine wrote.
use super::typetree::{Node, TypeTree};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

/// What `ScriptLayouts` prints.
#[derive(Deserialize, Default)]
pub struct Layouts {
    pub scripts: BTreeMap<String, Script>,
    pub types: BTreeMap<String, Composite>,
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

impl Templates {
    /// Takes the templates from script type trees the engine wrote: the header
    /// of the first, and each engine value type where a field holds one that
    /// is not hidden.
    pub fn of_trees<'a>(trees: impl IntoIterator<Item = &'a TypeTree>) -> Self {
        let mut templates = Templates::default();
        for tree in trees {
            if tree.class_id != super::typetree::SCRIPT_CLASS {
                continue;
            }
            let nodes = &tree.nodes;
            if templates.header.is_empty() {
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
                if node.level == 1
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
                if node.meta & HIDDEN != 0 {
                    continue;
                }
                for (csharp, tree_name, size) in ENGINE_VALUES {
                    if node.type_name == *tree_name
                        && (*size < -1 || node.size == *size)
                        && !templates.values.contains_key(*csharp)
                    {
                        templates
                            .values
                            .insert((*csharp).to_string(), subtree(nodes, index));
                    }
                }
            }
        }
        templates
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
