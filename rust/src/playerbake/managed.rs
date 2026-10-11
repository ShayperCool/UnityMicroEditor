//! Script-backed objects (class 114): the Player layout of a C# class and the
//! object a build writes for an authored `MonoBehaviour` document.
//!
//! A Player has no type tree for a script class on disk. The engine derives
//! one from the class's serialized fields, and a ContentFile has to carry the
//! same tree for its objects to be read. The fields come from assembly
//! metadata as a `ScriptLayout`; the tree, its type table entry and the object
//! bytes are derived here.
use super::{
    serialized::External,
    source::get,
    typetree::{TypeTree, Val, common_string},
    unsupported,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::borrow::Cow;

/// The serialized fields of a script class, base class fields first, as the
/// metadata reader reports them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptLayout {
    pub class_name: String,
    #[serde(default)]
    pub namespace: String,
    pub assembly: String,
    #[serde(default)]
    pub base_kind: String,
    /// Whether the class or a base class of the project overrides
    /// `OnStateMachineEnter` or `OnStateMachineExit`.
    #[serde(default)]
    pub overrides_state_machine_callbacks: bool,
    /// The classes the class derives from, nearest first, by full name and
    /// through the engine's own up to `UnityEngine.Object`. A field can refer
    /// to an object of the class if it declares the class or one of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<String>,
    /// Why the fields of the class cannot be laid out; empty if they can.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unsupported: String,
    #[serde(default)]
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Field {
    pub name: String,
    /// `[HideInInspector]`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// The names of `[FormerlySerializedAs]`, in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub former_names: Vec<String>,
    #[serde(rename = "type")]
    pub kind: FieldType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FieldType {
    /// A C# primitive, by its keyword.
    Primitive {
        name: String,
    },
    String,
    Enum {
        underlying: String,
    },
    /// A `[Serializable]` class or struct, stored inline.
    Struct {
        name: String,
        fields: Vec<Field>,
    },
    /// A struct the engine serializes itself, by its C# name.
    Unity {
        name: String,
    },
    /// A reference to an engine object.
    Object {
        #[serde(rename = "className")]
        class_name: String,
        #[serde(default)]
        namespace: String,
    },
    /// An array or a `List<T>`.
    Array {
        element: Box<FieldType>,
    },
    /// A C# `fixed` buffer.
    FixedBuffer {
        element: Box<FieldType>,
        length: usize,
    },
}

/// `ScriptLayout::base_kind` of a class the engine attaches to animator
/// states, and of any other scriptable object.
pub(super) const STATE_MACHINE_BEHAVIOUR: &str = "StateMachineBehaviour";
pub(super) const SCRIPTABLE_OBJECT: &str = "ScriptableObject";

// Transfer meta flags that occur in script type trees.
const HIDE: u32 = 0x1;
const STRONG_POINTER: u32 = 0x40;
const CHECK_BOX: u32 = 0x100;
const ALIGN: u32 = 0x4000;
const ANY_CHILD_ALIGNS: u32 = 0x8000;
const NAME: u32 = 0x8_0000;
const FLOW: u32 = 0x20_0000;
const BITWISE: u32 = 0x40_0000;
const DONT_ANIMATE: u32 = 0x80_0000;
const CHAR: u32 = 0x200_0000;
const FIXED_BUFFER: u32 = 0x800_0000;

/// A node of a type tree before it is flattened. The byte size and the
/// any-child-aligns flag of a parent follow from its children.
#[derive(Debug, Clone)]
struct Node {
    type_name: String,
    name: String,
    /// The byte size of a leaf.
    size: i32,
    meta: u32,
    version: u16,
    /// The `Array` node of a container.
    array: bool,
    children: Vec<Node>,
}

impl Node {
    fn leaf(type_name: &str, name: &str, size: i32, meta: u32) -> Self {
        Self {
            type_name: type_name.into(),
            name: name.into(),
            size,
            meta,
            version: 1,
            array: false,
            children: Vec::new(),
        }
    }

    fn group(type_name: &str, name: &str, meta: u32, children: Vec<Node>) -> Self {
        Self {
            children,
            ..Self::leaf(type_name, name, 0, meta)
        }
    }

    fn version(mut self, version: u16) -> Self {
        self.version = version;
        self
    }

    fn container(type_name: &str, name: &str, meta: u32, array_meta: u32, element: Node) -> Self {
        let array = Node {
            array: true,
            ..Node::group(
                "Array",
                "Array",
                array_meta,
                vec![Node::leaf("int", "size", 4, 0), element],
            )
        };
        Node::group(type_name, name, meta, vec![array])
    }

    fn string(name: &str, meta: u32) -> Self {
        Node::container(
            "string",
            name,
            meta,
            ALIGN | HIDE,
            Node::leaf("char", "data", 1, 0),
        )
    }

    fn scalars(
        type_name: &str,
        name: &str,
        meta: u32,
        element: &str,
        child_meta: u32,
        names: &[&str],
    ) -> Self {
        Node::group(
            type_name,
            name,
            meta,
            names
                .iter()
                .map(|child| Node::leaf(element, child, 4, child_meta))
                .collect(),
        )
    }

    fn floats(type_name: &str, name: &str, meta: u32, names: &[&str]) -> Self {
        Node::scalars(type_name, name, meta, "float", 0, names)
    }

    fn ints(type_name: &str, name: &str, meta: u32, names: &[&str]) -> Self {
        Node::scalars(type_name, name, meta, "int", 0, names)
    }

    fn pointer(type_name: &str, name: &str, meta: u32, child_meta: u32) -> Self {
        Node::group(
            type_name,
            name,
            meta,
            vec![
                Node::leaf("int", "m_FileID", 4, child_meta),
                Node::leaf("SInt64", "m_PathID", 8, child_meta),
            ],
        )
    }

    /// The element of a container node.
    fn element(&self) -> &Node {
        &self.children[0].children[1]
    }
}

/// Node type, byte size and field meta flags of a C# primitive.
fn primitive(name: &str) -> Option<(&'static str, i32, u32)> {
    Some(match name {
        "bool" => ("UInt8", 1, ALIGN | CHECK_BOX),
        "byte" => ("UInt8", 1, ALIGN),
        "sbyte" => ("SInt8", 1, ALIGN),
        "short" => ("SInt16", 2, ALIGN),
        "ushort" => ("UInt16", 2, ALIGN),
        "char" => ("UInt16", 2, ALIGN | CHAR),
        "int" => ("int", 4, 0),
        "uint" => ("unsigned int", 4, 0),
        "long" => ("SInt64", 8, 0),
        "ulong" => ("UInt64", 8, 0),
        "float" => ("float", 4, 0),
        "double" => ("double", 8, 0),
        _ => return None,
    })
}

/// The tree of a struct the engine serializes itself.
fn unity_node(kind: &str, name: &str) -> Result<Node> {
    const COLOR: [&str; 4] = ["r", "g", "b", "a"];
    const RECT: [&str; 4] = ["x", "y", "width", "height"];
    let color =
        |name: &str, meta: u32| Node::scalars("ColorRGBA", name, meta, "float", HIDE, &COLOR);
    let vector3 = |name: &str| Node::floats("Vector3f", name, FLOW, &["x", "y", "z"]);
    let int3 = |name: &str| Node::ints("int3_storage", name, FLOW, &["x", "y", "z"]);
    let bits = || Node::leaf("unsigned int", "m_Bits", 4, BITWISE | HIDE);
    Ok(match kind {
        "Vector2" => Node::floats("Vector2f", name, FLOW, &["x", "y"]),
        "Vector3" => vector3(name),
        "Vector4" => Node::floats("Vector4f", name, FLOW, &["x", "y", "z", "w"]),
        "Quaternion" => Node::floats("Quaternionf", name, FLOW, &["x", "y", "z", "w"]),
        "Color" => color(name, FLOW),
        "Color32" => Node::group(
            "ColorRGBA",
            name,
            0,
            vec![Node::leaf("unsigned int", "rgba", 4, HIDE)],
        )
        .version(2),
        "Rect" => Node::floats("Rectf", name, 0, &RECT).version(2),
        "Bounds" => Node::group(
            "AABB",
            name,
            0,
            vec![vector3("m_Center"), vector3("m_Extent")],
        ),
        "LayerMask" => Node::group("BitField", name, 0, vec![bits()]).version(2),
        "RenderingLayerMask" => Node::group("RenderingLayerMask", name, 0, vec![bits()]).version(0),
        "AnimationCurve" => {
            let float = |name: &str| Node::leaf("float", name, 4, 0);
            let key = Node::group(
                "Keyframe",
                "data",
                0,
                vec![
                    float("time"),
                    float("value"),
                    float("inSlope"),
                    float("outSlope"),
                    Node::leaf("int", "weightedMode", 4, 0),
                    float("inWeight"),
                    float("outWeight"),
                ],
            )
            .version(3);
            Node::group(
                "AnimationCurve",
                name,
                0,
                vec![
                    Node::container("vector", "m_Curve", HIDE, ALIGN, key),
                    Node::leaf("int", "m_PreInfinity", 4, HIDE),
                    Node::leaf("int", "m_PostInfinity", 4, HIDE),
                    Node::leaf("int", "m_RotationOrder", 4, HIDE),
                ],
            )
            .version(2)
        }
        "Gradient" => {
            let mut children: Vec<Node> = (0..8)
                .map(|index| color(&format!("key{index}"), FLOW | HIDE))
                .collect();
            for prefix in ["ctime", "atime"] {
                children.extend(
                    (0..8).map(|index| Node::leaf("UInt16", &format!("{prefix}{index}"), 2, HIDE)),
                );
            }
            children.extend([
                Node::leaf("UInt8", "m_Mode", 1, HIDE),
                Node::leaf("SInt8", "m_ColorSpace", 1, HIDE),
                Node::leaf("UInt8", "m_NumColorKeys", 1, HIDE),
                Node::leaf("UInt8", "m_NumAlphaKeys", 1, HIDE | ALIGN),
            ]);
            Node::group("Gradient", name, 0, children).version(2)
        }
        "Vector2Int" => Node::ints("int2_storage", name, FLOW, &["x", "y"]),
        "Vector3Int" => int3(name),
        "RectInt" => Node::ints("RectInt", name, 0, &RECT),
        "BoundsInt" => Node::group(
            "BoundsInt",
            name,
            0,
            vec![int3("m_Position"), int3("m_Size")],
        ),
        "Matrix4x4" => {
            let names: Vec<String> = (0..16)
                .map(|index| format!("e{}{}", index / 4, index % 4))
                .collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            Node::floats("Matrix4x4f", name, 0, &names)
        }
        "Hash128" => Node::group(
            "Hash128",
            name,
            0,
            (0..16)
                .map(|index| Node::leaf("UInt8", &format!("bytes[{index}]"), 1, 0))
                .collect(),
        ),
        "RectOffset" => Node::ints(
            "RectOffset",
            name,
            0,
            &["m_Left", "m_Right", "m_Top", "m_Bottom"],
        ),
        "SphericalHarmonicsL2" => {
            // The engine pads the index to two characters.
            let names: Vec<String> = (0..27).map(|index| format!("sh[{index:2}]")).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            Node::floats("SphericalHarmonicsL2", name, 0, &names)
        }
        // `LazyLoadReference<T>` of any `T`.
        "LazyLoadReference" => Node::pointer(
            "PPtr<$LazyLoadReference`1>",
            name,
            FIXED_BUFFER,
            DONT_ANIMATE | HIDE,
        ),
        _ => return Err(unsupported(format!("engine struct {kind} in a script"))),
    })
}

fn type_node(kind: &FieldType, name: &str) -> Result<Node> {
    Ok(match kind {
        FieldType::Primitive { name: keyword } => {
            let Some((type_name, size, meta)) = primitive(keyword) else {
                bail!("script layout names an unknown primitive {keyword}");
            };
            Node::leaf(type_name, name, size, meta)
        }
        FieldType::Enum { underlying } => {
            // The engine does not serialize an enum of a 64-bit type at all.
            let narrow = matches!(
                underlying.as_str(),
                "byte" | "sbyte" | "short" | "ushort" | "int" | "uint"
            );
            let Some((type_name, size, meta)) = primitive(underlying).filter(|_| narrow) else {
                bail!("script layout holds an enum over {underlying}");
            };
            Node::leaf(type_name, name, size, meta)
        }
        FieldType::String => Node::string(name, 0),
        FieldType::Unity { name: kind } => unity_node(kind, name)?,
        FieldType::Struct {
            name: type_name,
            fields,
        } => Node::group(
            type_name,
            name,
            0,
            fields.iter().map(field_node).collect::<Result<_>>()?,
        ),
        FieldType::Object { class_name, .. } => Node::pointer(
            &format!("PPtr<${class_name}>"),
            name,
            0,
            DONT_ANIMATE | HIDE,
        ),
        FieldType::Array { element } | FieldType::FixedBuffer { element, .. } => {
            let fixed = matches!(kind, FieldType::FixedBuffer { .. });
            let mut data = type_node(element, "data")?;
            match element.as_ref() {
                FieldType::Primitive { .. } | FieldType::Enum { .. } => {
                    // The element's own flags move up to the container.
                    let meta = std::mem::take(&mut data.meta);
                    let extra = if fixed { FIXED_BUFFER } else { 0 };
                    Node::container("vector", name, meta | extra, ALIGN, data)
                }
                _ if fixed => bail!("script layout holds a fixed buffer of a composite type"),
                FieldType::Array { .. } | FieldType::FixedBuffer { .. } => {
                    bail!("script layout holds an array of arrays")
                }
                // An array of an inline class is named after the class, and
                // its `Array` does not align.
                FieldType::Struct {
                    name: type_name, ..
                } => Node::container(type_name, name, 0, 0, data),
                FieldType::Object { .. } => Node::container("vector", name, 0, 0, data),
                FieldType::String | FieldType::Unity { .. } => {
                    Node::container("vector", name, 0, ALIGN, data)
                }
            }
        }
    })
}

fn field_node(field: &Field) -> Result<Node> {
    let mut node = type_node(&field.kind, &field.name)?;
    if field.hidden {
        node.meta |= HIDE;
    }
    Ok(node)
}

/// The fields every script object starts with in a Player.
const BASE_FIELDS: [&str; 4] = ["m_GameObject", "m_Enabled", "m_Script", "m_Name"];

fn root(layout: &ScriptLayout) -> Result<Node> {
    if !layout.unsupported.is_empty() {
        return Err(unsupported(format!(
            "script {}: {}",
            layout.class_name, layout.unsupported
        )));
    }
    // Both are scriptable objects, which a Player lays out alike.
    if !matches!(
        layout.base_kind.as_str(),
        STATE_MACHINE_BEHAVIOUR | SCRIPTABLE_OBJECT
    ) {
        return Err(unsupported(format!(
            "script {} is not a scriptable object",
            layout.class_name
        )));
    }
    let mut children = vec![
        Node::pointer(
            "PPtr<GameObject>",
            "m_GameObject",
            STRONG_POINTER | HIDE,
            HIDE,
        ),
        Node::leaf("UInt8", "m_Enabled", 1, ALIGN | CHECK_BOX | HIDE),
        Node::pointer("PPtr<MonoScript>", "m_Script", 0, DONT_ANIMATE | HIDE),
        Node::string("m_Name", NAME | HIDE),
    ];
    for field in &layout.fields {
        // The engine reads a field by name, so a second one of a name is
        // never reached.
        if children.iter().any(|known| known.name == field.name) {
            return Err(unsupported(format!(
                "script {} serializes two fields named {}",
                layout.class_name, field.name
            )));
        }
        children.push(field_node(field)?);
    }
    Ok(Node::group("MonoBehaviour", "Base", 0, children))
}

struct Flat<'a> {
    node: &'a Node,
    level: u8,
    size: i32,
    meta: u32,
}

/// Lists the nodes depth first and derives what a parent takes from its
/// children: the byte size, unknown once a container is involved, and the
/// flag that says a descendant aligns.
fn flatten<'a>(node: &'a Node, level: usize, out: &mut Vec<Flat<'a>>) -> Result<(i32, u32)> {
    if level > 250 {
        bail!("script layout nests deeper than a type tree can hold");
    }
    let index = out.len();
    out.push(Flat {
        node,
        level: level as u8,
        size: node.size,
        meta: node.meta,
    });
    let (mut size, mut aligned) = (0i32, false);
    for child in &node.children {
        let (child_size, child_meta) = flatten(child, level + 1, out)?;
        aligned |= child_meta & (ALIGN | ANY_CHILD_ALIGNS) != 0;
        size = if size < 0 || child_size < 0 {
            -1
        } else {
            size + child_size
        };
    }
    if !node.children.is_empty() {
        out[index].size = if node.array { -1 } else { size };
    }
    if aligned {
        out[index].meta |= ANY_CHILD_ALIGNS;
    }
    Ok((out[index].size, out[index].meta))
}

/// The type tree blob: a header, 32 bytes per node, then the strings the
/// engine's common table lacks, each once, in order of first use.
fn blob(nodes: &[Flat<'_>]) -> Vec<u8> {
    let mut local: Vec<u8> = Vec::new();
    let mut offsets: Vec<(&str, u32)> = Vec::new();
    let mut reference = |text| -> u32 {
        if let Some(offset) = common_string(text) {
            return offset | 0x8000_0000;
        }
        if let Some((_, offset)) = offsets.iter().find(|(known, _)| *known == text) {
            return *offset;
        }
        let offset = local.len() as u32;
        local.extend_from_slice(text.as_bytes());
        local.push(0);
        offsets.push((text, offset));
        offset
    };
    let mut records = Vec::with_capacity(nodes.len() * 32);
    for (index, flat) in nodes.iter().enumerate() {
        records.extend_from_slice(&flat.node.version.to_le_bytes());
        records.push(flat.level);
        records.push(flat.node.array as u8);
        records.extend_from_slice(&reference(flat.node.type_name.as_str()).to_le_bytes());
        records.extend_from_slice(&reference(flat.node.name.as_str()).to_le_bytes());
        records.extend_from_slice(&flat.size.to_le_bytes());
        records.extend_from_slice(&(index as u32).to_le_bytes());
        records.extend_from_slice(&flat.meta.to_le_bytes());
        records.extend_from_slice(&0u64.to_le_bytes());
    }
    let mut out = b"mhtt".to_vec();
    out.extend_from_slice(&23u32.to_le_bytes());
    out.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
    out.extend_from_slice(&(local.len() as u32).to_le_bytes());
    out.extend_from_slice(&records);
    out.extend_from_slice(&local);
    out
}

/// MD4 (RFC 1320), which the engine uses for the identity of a script class
/// and of its layout.
fn md4(data: &[u8]) -> [u8; 16] {
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&(data.len() as u64 * 8).to_le_bytes());
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in message.as_chunks::<64>().0 {
        let mut words = [0u32; 16];
        for (word, bytes) in words.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*bytes);
        }
        let [mut a, mut b, mut c, mut d] = state;
        for index in 0..48 {
            let (mixed, word, shift) = match index / 16 {
                0 => ((b & c) | (!b & d), index, [3, 7, 11, 19][index % 4]),
                1 => (
                    ((b & c) | (b & d) | (c & d)).wrapping_add(0x5a82_7999),
                    (index % 4) * 4 + (index % 16) / 4,
                    [3, 5, 9, 13][index % 4],
                ),
                _ => (
                    (b ^ c ^ d).wrapping_add(0x6ed9_eba1),
                    [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15][index % 16],
                    [3, 9, 11, 15][index % 4],
                ),
            };
            let next = a
                .wrapping_add(mixed)
                .wrapping_add(words[word])
                .rotate_left(shift);
            (a, b, c, d) = (d, next, b, c);
        }
        for (sum, value) in state.iter_mut().zip([a, b, c, d]) {
            *sum = sum.wrapping_add(value);
        }
    }
    let mut digest = [0u8; 16];
    for (bytes, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *bytes = word.to_le_bytes();
    }
    digest
}

/// The Player layout of a script class.
pub struct ScriptType {
    /// The class's entry of a type table. Its script type index is zero; the
    /// file writer sets it.
    pub tree: TypeTree,
    /// The hash of the layout, which the class's `MonoScript` object repeats.
    pub properties_hash: [u8; 16],
    fields: Vec<Field>,
    root: Node,
}

impl ScriptType {
    pub fn new(layout: &ScriptLayout) -> Result<Self> {
        let root = root(layout)?;
        let mut nodes = Vec::new();
        flatten(&root, 0, &mut nodes)?;
        let blob = blob(&nodes);
        // The engine hashes the class's name parts with nothing in between.
        let mut identity = Vec::new();
        for part in [&layout.class_name, &layout.namespace, &layout.assembly] {
            identity.extend_from_slice(part.as_bytes());
        }
        let script_id = md4(&identity);
        for flat in &nodes {
            identity.extend_from_slice(flat.node.type_name.as_bytes());
            identity.extend_from_slice(flat.node.name.as_bytes());
            identity.extend_from_slice(&flat.size.to_le_bytes());
            identity.extend_from_slice(&(flat.node.array as i32).to_le_bytes());
            identity.extend_from_slice(&(flat.node.version as i32).to_le_bytes());
            identity.extend_from_slice(&(flat.meta & ALIGN).to_le_bytes());
        }
        let properties_hash = md4(&identity);
        let mut entry = Vec::with_capacity(blob.len() + 64);
        entry.extend_from_slice(&114i32.to_le_bytes());
        entry.push(0); // Not stripped.
        entry.extend_from_slice(&0i16.to_le_bytes());
        entry.extend_from_slice(&script_id);
        entry.extend_from_slice(&properties_hash);
        // A Player that loads a ContentFile checks none of the three hashes;
        // they are written as a build writes them all the same.
        entry.extend_from_slice(&xxhash_rust::xxh3::xxh3_128(&blob).to_le_bytes());
        entry.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        entry.extend_from_slice(&blob);
        entry.extend_from_slice(&0u32.to_le_bytes()); // No type dependencies.
        Ok(Self {
            tree: TypeTree::parse(entry)?,
            properties_hash,
            fields: layout.fields.clone(),
            root,
        })
    }

    /// The object a build writes for an authored behaviour document; `fields`
    /// is the document's mapping and `script_local_id` the ID of the class's
    /// `MonoScript` object in the same file.
    ///
    /// A document that lacks a serialized field is refused: the engine would
    /// keep what the class's constructor left in it, which no layout tells.
    pub fn object(
        &self,
        fields: &Map<String, Value>,
        script_local_id: i64,
        references: &mut dyn References,
    ) -> Result<Encoded> {
        let Some(enabled) = fields.get("m_Enabled") else {
            return Err(unsupported(
                "behaviour field m_Enabled is not in the authored document",
            ));
        };
        let enabled = integer("UInt8", enabled, "m_Enabled")?;
        let mut values = vec![
            (Cow::Borrowed("m_Enabled"), enabled),
            (
                Cow::Borrowed("m_Script"),
                Val::map([("m_PathID", Val::I(script_local_id))]),
            ),
            (
                Cow::Borrowed("m_Name"),
                Val::Str(string(get(fields, "m_Name"), "m_Name")?),
            ),
        ];
        let nodes = &self.root.children[BASE_FIELDS.len()..];
        let mut encoder = Encoder {
            references,
            deferred: Vec::new(),
        };
        for (field, node) in self.fields.iter().zip(nodes) {
            values.push((
                Cow::Owned(field.name.clone()),
                encoder.field(field, node, fields, "")?,
            ));
        }
        Ok(Encoded {
            data: self.tree.encode(&Val::Map(values))?,
            deferred: encoder.deferred,
        })
    }
}

/// An object reference a script field holds, as authored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectReference<'a> {
    /// The asset that holds the object, in lower case; empty for an object
    /// of the file the reference stands in.
    pub guid: &'a str,
    pub file_id: i64,
    /// The authored `type`: 2 for an asset stored in Unity's own format, 3
    /// for an imported one, 0 for a resource built into the engine.
    pub kind: i64,
    /// The class the field declares.
    pub class_name: &'a str,
    pub namespace: &'a str,
    /// Where the field is in its object: `field`, `nested.field`, `list[3]`
    /// or `list[3].field`, as `JsonUtility` names it.
    pub path: &'a str,
}

/// What an object reference becomes in a baked object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pointer {
    /// The file, as the position of an external reference counted from one,
    /// or zero for the object's own file, and the object's ID in it.
    Object {
        file: i64,
        path_id: i64,
    },
    Null,
    /// Written as null and reported, for the caller to bind once the object
    /// is loaded.
    Deferred,
}

/// Resolves the object references a script object's fields hold.
pub trait References {
    /// The pointer for a reference that is not null.
    fn pointer(&mut self, reference: &ObjectReference<'_>) -> Result<Pointer>;
}

/// A reference that was written as null on the resolver's word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredReference {
    /// The field's path, as in `ObjectReference::path`.
    pub path: String,
    pub guid: String,
    pub file_id: i64,
    pub kind: i64,
}

/// A baked script object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    pub data: Vec<u8>,
    /// The references left null, in field order.
    pub deferred: Vec<DeferredReference>,
}

/// Where the object behind a reference to another asset is at run time. This
/// is the answer of the caller, who knows what else is published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// An object of another ContentFile: the GUID that file is published
    /// under, which becomes an external reference, and the object's ID in it.
    External { guid: String, local_id: i64 },
    /// No ContentFile holds the object. The field is written as null and
    /// the reference is returned as a deferred binding.
    Deferred,
    /// The reference leads nowhere, as the engine would find it.
    Null,
}

/// A deferred reference of one object of a baked file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredBinding {
    /// The local file ID of the object whose field was left null.
    pub owner: String,
    /// The field's path in that object: `field`, `nested.field`, `list[3]`
    /// or `list[3].field`.
    pub path: String,
    pub guid: String,
    pub file_id: i64,
    pub kind: i64,
}

/// Resolves references for the objects of one file: `local` answers for an
/// object of the file itself, `resolve` for one of another asset.
pub(super) struct FileReferences<'a> {
    pub local: &'a dyn Fn(&ObjectReference<'_>) -> Result<Pointer>,
    pub resolve: &'a dyn Fn(&ObjectReference<'_>) -> Result<Resolution>,
    pub externals: &'a mut Vec<External>,
}

impl References for FileReferences<'_> {
    fn pointer(&mut self, reference: &ObjectReference<'_>) -> Result<Pointer> {
        if reference.guid.is_empty() {
            return (self.local)(reference);
        }
        Ok(match (self.resolve)(reference)? {
            Resolution::External { guid, local_id } => {
                let position = match self.externals.iter().position(|known| known.guid == guid) {
                    Some(position) => position,
                    None => {
                        self.externals.push(External { guid });
                        self.externals.len() - 1
                    }
                };
                Pointer::Object {
                    file: position as i64 + 1,
                    path_id: local_id,
                }
            }
            Resolution::Deferred => Pointer::Deferred,
            Resolution::Null => Pointer::Null,
        })
    }
}

struct Encoder<'a> {
    references: &'a mut dyn References,
    deferred: Vec<DeferredReference>,
}

fn mismatch(path: &str, what: &str) -> anyhow::Error {
    unsupported(format!(
        "behaviour field {path} is authored as {what}, which its class does not declare"
    ))
}

fn scalar_text<'a>(value: &'a Value, path: &str) -> Result<&'a str> {
    match value {
        Value::Null => Ok(""),
        Value::String(text) => Ok(text.trim_matches(' ')),
        _ => Err(mismatch(path, "a structure")),
    }
}

fn string(value: &Value, path: &str) -> Result<String> {
    match value {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.clone()),
        _ => Err(mismatch(path, "a structure")),
    }
}

/// Byte size and signedness of an integer node type.
fn integer_type(type_name: &str) -> Option<(u32, bool)> {
    Some(match type_name {
        "UInt8" => (1, false),
        "SInt8" => (1, true),
        "UInt16" => (2, false),
        "SInt16" => (2, true),
        "unsigned int" => (4, false),
        "int" => (4, true),
        "UInt64" => (8, false),
        "SInt64" => (8, true),
        _ => return None,
    })
}

/// The engine keeps the low bits of the integer the text holds.
fn truncated(type_name: &str, number: i128) -> Val {
    let (size, signed) = integer_type(type_name).unwrap_or((8, false));
    let bits = size * 8;
    let low = (number as u64) & (u64::MAX >> (64 - bits));
    if signed {
        Val::I(((low << (64 - bits)) as i64) >> (64 - bits))
    } else {
        Val::U(low)
    }
}

fn integer(type_name: &str, value: &Value, path: &str) -> Result<Val> {
    let text = scalar_text(value, path)?;
    let number = match text {
        "" => 0,
        text => match text.parse::<i128>() {
            Ok(number) => number,
            Err(_) => return Err(mismatch(path, "text that is not an integer")),
        },
    };
    Ok(truncated(type_name, number))
}

/// A float as the Editor writes it. Its three names for values that are not
/// finite are read as such; any other text has to be a number.
fn float<T: Copy + std::str::FromStr + Into<f64> + From<f32>>(
    value: &Value,
    path: &str,
) -> Result<T> {
    let text = scalar_text(value, path)?;
    let special = match text {
        "" => Some(0.0),
        "NaN" => Some(f32::NAN),
        "Infinity" => Some(f32::INFINITY),
        "-Infinity" => Some(f32::NEG_INFINITY),
        _ => None,
    };
    if let Some(special) = special {
        return Ok(special.into());
    }
    match text.parse::<T>() {
        Ok(number) if !number.into().is_nan() => Ok(number),
        _ => Err(mismatch(path, "text that is not a number")),
    }
}

fn scalar(type_name: &str, value: &Value, path: &str) -> Result<Val> {
    match type_name {
        "float" => Ok(Val::F(float(value, path)?)),
        "double" => Ok(Val::D(float(value, path)?)),
        _ => integer(type_name, value, path),
    }
}

/// The type letter the Editor appends to the hex dump of an integer array.
fn dump_suffix(type_name: &str) -> Option<char> {
    Some(match type_name {
        "UInt8" => 'y',
        "SInt8" => 'n',
        "SInt16" => 's',
        "UInt16" => 't',
        "int" => 'i',
        "unsigned int" => 'u',
        "SInt64" => 'l',
        "UInt64" => 'q',
        _ => return None,
    })
}

/// The `serializedVersion` the Editor writes into the YAML of an engine
/// struct. An older form has other fields, which the engine converts.
fn authored_version(node: &Node) -> u16 {
    match node.type_name.as_str() {
        // Text holds the hash as one hex string, a form of its own.
        "Hash128" => 2,
        _ => node.version,
    }
}

impl Encoder<'_> {
    /// One field of a C# type, found by its name or a former name.
    fn field(
        &mut self,
        field: &Field,
        node: &Node,
        mapping: &Map<String, Value>,
        path: &str,
    ) -> Result<Val> {
        let path = format!("{path}{}", field.name);
        let names = std::iter::once(&field.name).chain(&field.former_names);
        let Some(value) = names.into_iter().find_map(|name| mapping.get(name)) else {
            // The Editor writes nothing for a type without fields.
            if matches!(&field.kind, FieldType::Struct { fields, .. } if fields.is_empty()) {
                return Ok(Val::Zero);
            }
            return Err(unsupported(format!(
                "behaviour field {path} is not in the authored document"
            )));
        };
        self.value(&field.kind, node, value, &path)
    }

    fn pointer(
        &mut self,
        class_name: &str,
        namespace: &str,
        value: &Value,
        path: &str,
    ) -> Result<Val> {
        let reference = match value {
            Value::Null => return Ok(Val::Zero),
            Value::String(text) if text.is_empty() => return Ok(Val::Zero),
            Value::Object(reference) => reference,
            _ => return Err(mismatch(path, "a value")),
        };
        let file_id = scalar_text(get(reference, "fileID"), path)?;
        if file_id.is_empty() || file_id == "0" {
            return Ok(Val::Zero);
        }
        let Ok(file_id) = file_id.parse::<i64>() else {
            return Err(mismatch(path, "a reference without a file ID"));
        };
        let guid = scalar_text(get(reference, "guid"), path)?.to_ascii_lowercase();
        let kind = match scalar_text(get(reference, "type"), path)? {
            "" => 0,
            kind => match kind.parse::<i64>() {
                Ok(kind) => kind,
                Err(_) => return Err(mismatch(path, "a reference of an unknown type")),
            },
        };
        let pointer = self.references.pointer(&ObjectReference {
            guid: &guid,
            file_id,
            kind,
            class_name,
            namespace,
            path,
        })?;
        Ok(match pointer {
            Pointer::Object { file, path_id } => {
                Val::map([("m_FileID", Val::I(file)), ("m_PathID", Val::I(path_id))])
            }
            Pointer::Null => Val::Zero,
            Pointer::Deferred => {
                self.deferred.push(DeferredReference {
                    path: path.to_string(),
                    guid,
                    file_id,
                    kind,
                });
                Val::Zero
            }
        })
    }

    fn value(&mut self, kind: &FieldType, node: &Node, value: &Value, path: &str) -> Result<Val> {
        match kind {
            FieldType::Primitive { .. } | FieldType::Enum { .. } => {
                scalar(&node.type_name, value, path)
            }
            FieldType::String => Ok(Val::Str(string(value, path)?)),
            FieldType::Object {
                class_name,
                namespace,
            } => self.pointer(class_name, namespace, value, path),
            FieldType::Array { element } | FieldType::FixedBuffer { element, .. } => {
                let data = node.element();
                let mut items = match value {
                    Value::Null => Vec::new(),
                    Value::String(text) if text.is_empty() => Vec::new(),
                    // An array of integers is one scalar: the elements as
                    // little-endian hex, then a letter for their type.
                    Value::String(text) => {
                        let Some((size, _)) = integer_type(&data.type_name) else {
                            return Err(mismatch(path, "a value"));
                        };
                        let digits = text
                            .strip_suffix(|last| dump_suffix(&data.type_name) == Some(last))
                            .unwrap_or(text);
                        let width = size as usize * 2;
                        if !digits.is_ascii() || !digits.len().is_multiple_of(width) {
                            return Err(mismatch(path, "a hex dump of another width"));
                        }
                        let mut items = Vec::with_capacity(digits.len() / width);
                        for chunk in digits.as_bytes().chunks(width) {
                            let mut number = 0u64;
                            for (index, pair) in chunk.chunks(2).enumerate() {
                                let pair = std::str::from_utf8(pair).unwrap_or("");
                                let Ok(byte) = u8::from_str_radix(pair, 16) else {
                                    return Err(mismatch(path, "text that is not a hex dump"));
                                };
                                number |= (byte as u64) << (8 * index);
                            }
                            items.push(truncated(&data.type_name, number as i128));
                        }
                        items
                    }
                    Value::Array(authored) => authored
                        .iter()
                        .enumerate()
                        .map(|(index, item)| {
                            self.value(element, data, item, &format!("{path}[{index}]"))
                        })
                        .collect::<Result<_>>()?,
                    _ => return Err(mismatch(path, "a structure")),
                };
                // A fixed buffer always has its declared length.
                if let FieldType::FixedBuffer { length, .. } = kind {
                    items.resize(*length, Val::Zero);
                }
                Ok(Val::Seq(items))
            }
            FieldType::Struct { fields, .. } => {
                let empty = Map::new();
                let mapping = match value {
                    Value::Object(mapping) => mapping,
                    // A type without fields has nothing to author.
                    _ if fields.is_empty() => &empty,
                    _ => return Err(mismatch(path, "a value")),
                };
                let mut values = Vec::with_capacity(fields.len());
                for (field, child) in fields.iter().zip(&node.children) {
                    values.push((
                        Cow::Owned(field.name.clone()),
                        self.field(field, child, mapping, &format!("{path}."))?,
                    ));
                }
                Ok(Val::Map(values))
            }
            FieldType::Unity { .. } if node.type_name.starts_with("PPtr<") => {
                self.pointer("", "", value, path)
            }
            FieldType::Unity { .. } => self.engine_struct(node, value, path),
        }
    }

    /// A struct the engine serializes itself: the authored keys are the node
    /// names. Every value has to be there, in the current form of the struct.
    fn engine_struct(&mut self, node: &Node, value: &Value, path: &str) -> Result<Val> {
        let Value::Object(mapping) = value else {
            return Err(mismatch(path, "a value"));
        };
        let expected = authored_version(node);
        let current = match mapping.get("serializedVersion") {
            Some(version) => scalar_text(version, path)?.parse::<u16>().ok() == Some(expected),
            // The first form of a struct is written without a version.
            None => expected <= 1,
        };
        if !current {
            return Err(unsupported(format!(
                "behaviour field {path} is authored in another version of {}",
                node.type_name
            )));
        }
        if node.type_name == "Hash128" {
            let dump = scalar_text(get(mapping, "Hash"), path)?;
            let bytes: Option<Vec<Val>> = (0..16)
                .map(|index| {
                    dump.get(index * 2..index * 2 + 2)
                        .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                        .map(|byte| Val::U(byte as u64))
                })
                .collect();
            let (Some(bytes), 32) = (bytes, dump.len()) else {
                return Err(mismatch(path, "text that is not a hash"));
            };
            return Ok(Val::Map(
                node.children
                    .iter()
                    .zip(bytes)
                    .map(|(child, byte)| (Cow::Owned(child.name.clone()), byte))
                    .collect(),
            ));
        }
        let mut values = Vec::with_capacity(node.children.len());
        for child in &node.children {
            let path = format!("{path}.{}", child.name);
            let Some(authored) = mapping.get(&child.name) else {
                return Err(unsupported(format!(
                    "behaviour field {path} is not in the authored document"
                )));
            };
            let value = if child.children.is_empty() {
                scalar(&child.type_name, authored, &path)?
            } else if child.children[0].array {
                let items = match authored {
                    Value::Array(items) => items.as_slice(),
                    Value::Null => &[],
                    Value::String(text) if text.is_empty() => &[],
                    _ => return Err(mismatch(&path, "a value")),
                };
                Val::Seq(
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| {
                            self.engine_struct(child.element(), item, &format!("{path}[{index}]"))
                        })
                        .collect::<Result<_>>()?,
                )
            } else {
                self.engine_struct(child, authored, &path)?
            };
            values.push((Cow::Owned(child.name.clone()), value));
        }
        Ok(Val::Map(values))
    }
}

#[cfg(test)]
mod tests {
    use super::md4;

    fn hex(digest: [u8; 16]) -> String {
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn md4_matches_the_rfc_vectors() {
        assert_eq!(hex(md4(b"")), "31d6cfe0d16ae931b73c59d7e0c089c0");
        assert_eq!(hex(md4(b"abc")), "a448017aaf21d8525fc10ae87aa6729d");
        assert_eq!(
            hex(md4(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "e33b4ddc9c38f2199c3e7b164fcc0536"
        );
    }
}
