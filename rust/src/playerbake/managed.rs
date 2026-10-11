//! Script-backed objects (class 114): the Player layout of a C# class and the
//! object a build writes for an authored `MonoBehaviour` document.
//!
//! A Player has no type tree for a script class on disk. The engine derives
//! one from the class's serialized fields, and a ContentFile has to carry the
//! same tree for its objects to be read. The fields come from assembly
//! metadata as a `ScriptLayout`; the tree, its type table entry and the object
//! bytes are derived here.
use super::{
    crc32,
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
    /// The classes of the project an object held by a managed reference of
    /// the class may be of.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managed_types: Vec<ManagedLayout>,
}

/// The serialized fields of a class that is held by managed reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedLayout {
    /// The class as text names it: a nested class after the classes around
    /// it, `Outer/Inner`.
    pub class_name: String,
    #[serde(default)]
    pub namespace: String,
    pub assembly: String,
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
    /// The constant the constructor of the class stores in the field, as
    /// the engine's text would hold it. Absent for a field the constructor
    /// leaves as the engine makes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<String>,
    /// The constructor stores something the reader does not follow.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub initial_unknown: bool,
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
    /// A `[SerializeReference]` field: the ID of an object in the registry
    /// that the object holding the field ends with. `name` is the declared
    /// type, after which a list of such fields is named.
    ManagedReference {
        #[serde(default)]
        name: String,
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

// Type flags of the nodes that have to do with managed references. An array
// has flag 1.
const MANAGED_REFERENCE: u8 = 2;
const REGISTRY: u8 = 4;
const REFERENCE_ARRAY: u8 = 8;

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
    /// The type flags other than that of an array.
    flags: u8,
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
            flags: 0,
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

    /// The ID of an object held by managed reference.
    fn reference(type_name: &str, name: &str) -> Self {
        Node {
            flags: MANAGED_REFERENCE,
            ..Node::group(type_name, name, 0, vec![Node::leaf("SInt64", "rid", 8, 0)])
        }
    }

    /// The registry an object ends with that holds others by managed
    /// reference: each with its ID, its class and, in the object's bytes,
    /// its data, which the tree of the class describes.
    fn registry() -> Self {
        let object = Node::group(
            "ReferencedObject",
            "data",
            0,
            vec![
                Node::leaf("SInt64", "rid", 8, 0),
                Node::group(
                    "ReferencedManagedType",
                    "type",
                    FLOW,
                    vec![
                        Node::string("class", 0),
                        Node::string("ns", 0),
                        Node::string("asm", 0),
                    ],
                ),
                Node::group("ReferencedObjectData", "data", 0, Vec::new()),
            ],
        );
        Node {
            flags: REGISTRY,
            ..Node::group(
                "ManagedReferencesRegistry",
                REGISTRY_FIELD,
                HIDE,
                vec![
                    Node::leaf("int", "version", 4, 0),
                    Node::container("vector", "RefIds", 0, ALIGN, object),
                ],
            )
        }
    }
}

const REGISTRY_FIELD: &str = "references";

/// Whether fields, or the classes stored inline in them, hold managed
/// references.
fn hold_references(fields: &[Field]) -> bool {
    fn holds(kind: &FieldType) -> bool {
        match kind {
            FieldType::ManagedReference { .. } => true,
            FieldType::Struct { fields, .. } => hold_references(fields),
            FieldType::Array { element } | FieldType::FixedBuffer { element, .. } => holds(element),
            _ => false,
        }
    }
    fields.iter().any(|field| holds(&field.kind))
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
        "GUID" => Node::scalars(
            "GUID",
            name,
            0,
            "unsigned int",
            0,
            &["data[0]", "data[1]", "data[2]", "data[3]"],
        ),
        // A node of the type of a string that holds one.
        "PropertyName" => Node::group("string", name, 0, vec![Node::string("id", ALIGN)]),
        "GUIStyle" => {
            let pointer = |class: &str, name: &str| {
                Node::pointer(&format!("PPtr<{class}>"), name, 0, DONT_ANIMATE | HIDE)
            };
            let flag = |name: &str, meta: u32| Node::leaf("bool", name, 1, meta);
            let int = |name: &str| Node::leaf("int", name, 4, 0);
            let float = |name: &str| Node::leaf("float", name, 4, 0);
            let mut children = vec![Node::string("m_Name", ALIGN)];
            for state in [
                "m_Normal",
                "m_Hover",
                "m_Active",
                "m_Focused",
                "m_OnNormal",
                "m_OnHover",
                "m_OnActive",
                "m_OnFocused",
            ] {
                children.push(Node::group(
                    "GUIStyleState",
                    state,
                    0,
                    vec![
                        pointer("Texture2D", "m_Background"),
                        color("m_TextColor", FLOW),
                    ],
                ));
            }
            for offset in ["m_Border", "m_Margin", "m_Padding", "m_Overflow"] {
                children.push(unity_node("RectOffset", offset)?);
            }
            children.extend([
                pointer("Font", "m_Font"),
                int("m_FontSize"),
                int("m_FontStyle"),
                int("m_Alignment"),
                flag("m_WordWrap", 0),
                flag("m_RichText", ALIGN),
                int("m_TextClipping"),
                int("m_ImagePosition"),
                Node::floats("Vector2f", "m_ContentOffset", FLOW, &["x", "y"]),
                float("m_ContentSpacing"),
                float("m_FixedWidth"),
                float("m_FixedHeight"),
                flag("m_ImageIsTopAligned", 0),
                flag("m_StretchWidth", 0),
                flag("m_StretchHeight", 0),
                flag("m_IsSDF", ALIGN),
            ]);
            Node::group("GUIStyle", name, 0, children)
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
        FieldType::ManagedReference { .. } => Node::reference("managedReference", name),
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
                // A list of managed references is named after the type it
                // is declared with.
                FieldType::ManagedReference { name: declared } => Node {
                    flags: REFERENCE_ARRAY,
                    ..Node::container(
                        declared,
                        name,
                        0,
                        0,
                        Node::reference("managedRefArrayItem", "data"),
                    )
                },
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
    if hold_references(&layout.fields) {
        if children.iter().any(|known| known.name == REGISTRY_FIELD) {
            return Err(unsupported(format!(
                "script {} holds managed references and a field named {REGISTRY_FIELD}",
                layout.class_name
            )));
        }
        children.push(Node::registry());
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
        records.push(flat.node.array as u8 | flat.node.flags);
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
    managed: Vec<ManagedType>,
}

/// A class of which script objects hold objects by managed reference.
pub struct ManagedType {
    /// The class as text names it, a nested class as `Outer/Inner`.
    pub class_name: String,
    pub namespace: String,
    pub assembly: String,
    /// The type tree of the class, as a file's table of such types holds it.
    pub blob: Vec<u8>,
    /// Why the class cannot be laid out; empty if it can.
    unsupported: String,
    fields: Vec<Field>,
    /// The tree that the data of an object is written with: the fields
    /// without the registry that the full tree of a class ends with if the
    /// class holds managed references itself.
    data: Option<(Node, TypeTree)>,
}

impl ManagedType {
    fn new(layout: &ManagedLayout) -> Result<Self> {
        let mut kind = Self {
            class_name: layout.class_name.clone(),
            namespace: layout.namespace.clone(),
            assembly: layout.assembly.clone(),
            blob: Vec::new(),
            unsupported: layout.unsupported.clone(),
            fields: layout.fields.clone(),
            data: None,
        };
        if !kind.unsupported.is_empty() {
            return Ok(kind);
        }
        let build = || -> Result<(Vec<u8>, Node, TypeTree)> {
            // A nested class is named without the classes around it.
            let name = layout.class_name.rsplit('/').next().unwrap_or_default();
            let mut children = Vec::with_capacity(layout.fields.len());
            for field in &layout.fields {
                if children.iter().any(|known: &Node| known.name == field.name) {
                    return Err(unsupported(format!(
                        "two serialized fields named {}",
                        field.name
                    )));
                }
                children.push(field_node(field)?);
            }
            let data = Node::group(name, "Base", 0, children);
            let mut full = data.clone();
            if hold_references(&layout.fields) {
                full.children.push(Node::registry());
            }
            let mut nodes = Vec::new();
            flatten(&full, 0, &mut nodes)?;
            let full = blob(&nodes);
            let mut nodes = Vec::new();
            flatten(&data, 0, &mut nodes)?;
            let tree = blob(&nodes);
            // An entry as the tree reader takes it, of no engine class.
            let mut entry = Vec::with_capacity(tree.len() + 47);
            entry.extend_from_slice(&(-1i32).to_le_bytes());
            entry.extend_from_slice(&[0u8; 35]);
            entry.extend_from_slice(&(tree.len() as u32).to_le_bytes());
            entry.extend_from_slice(&tree);
            entry.extend_from_slice(&0u32.to_le_bytes());
            Ok((full, data, TypeTree::parse(entry)?))
        };
        match build() {
            Ok((blob, root, tree)) => {
                kind.blob = blob;
                kind.data = Some((root, tree));
            }
            // A class that cannot be laid out is refused once an object
            // holds one of it.
            Err(error) => kind.unsupported = error.downcast::<super::Unsupported>()?.to_string(),
        }
        Ok(kind)
    }
}

impl ScriptType {
    /// The classes of which objects of the class may hold objects by managed
    /// reference; `Encoded::managed` names the ones an object does hold.
    pub fn managed_types(&self) -> &[ManagedType] {
        &self.managed
    }

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
            let flags = flat.node.array as u8 | flat.node.flags;
            identity.extend_from_slice(&(flags as i32).to_le_bytes());
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
        let managed = layout
            .managed_types
            .iter()
            .map(ManagedType::new)
            .collect::<Result<_>>()?;
        Ok(Self {
            tree: TypeTree::parse(entry)?,
            properties_hash,
            fields: layout.fields.clone(),
            root,
            managed,
        })
    }

    /// The object a build writes for an authored behaviour document; `fields`
    /// is the document's mapping and `script_local_id` the ID of the class's
    /// `MonoScript` object in the same file.
    ///
    /// The engine reads a document over a newly constructed object, so a
    /// field the document lacks keeps what the class's constructor left in
    /// it. Such a field is filled in where the layout tells that value and
    /// refused where it does not.
    pub fn object(
        &self,
        fields: &Map<String, Value>,
        script_local_id: i64,
        references: &mut dyn References,
    ) -> Result<Encoded> {
        // A new object is enabled.
        let enabled = match fields.get("m_Enabled") {
            Some(enabled) => integer("UInt8", enabled, "m_Enabled")?,
            None => Val::U(1),
        };
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
            held: Vec::new(),
        };
        for (field, node) in self.fields.iter().zip(nodes) {
            values.push((
                Cow::Owned(field.name.clone()),
                encoder.field(field, node, fields, "", true)?,
            ));
        }
        let mut data = self.tree.encode(&Val::Map(values))?;
        let mut managed = Vec::new();
        if hold_references(&self.fields) {
            // The tree ends in the registry, written empty so far: a
            // version and a count. The data of an object of the registry is
            // laid out by the tree of its class, which the tree of the
            // registry leaves open.
            let Some(start) = data.len().checked_sub(8) else {
                bail!("script object is shorter than its registry");
            };
            data.truncate(start);
            managed = encoder.registry(get(fields, REGISTRY_FIELD), &self.managed, &mut data)?;
        }
        Ok(Encoded {
            data,
            deferred: encoder.deferred,
            managed,
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
    /// The classes of the objects it holds by managed reference, as
    /// positions in `ScriptType::managed_types`.
    pub managed: Vec<usize>,
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
    /// The IDs of the objects the fields read so far hold by managed
    /// reference.
    held: Vec<i64>,
}

/// The ID a managed reference holds: that of an object of the registry, or
/// -2 for none.
fn managed_id(value: &Value, path: &str) -> Result<i64> {
    let Value::Object(reference) = value else {
        return Err(mismatch(path, "a value"));
    };
    match scalar_text(get(reference, "rid"), path)?.parse::<i64>() {
        Ok(id) if id >= -2 && id != -1 => Ok(id),
        // -1 stands for an object whose class is gone.
        _ => Err(mismatch(path, "a managed reference without an object")),
    }
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

/// The engine reads an integer as far as the text is one: a sign and the
/// digits that follow, wrapping at 64 bits; nothing at all reads as zero.
fn integer(type_name: &str, value: &Value, path: &str) -> Result<Val> {
    let text = scalar_text(value, path)?;
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let mut number = 0u64;
    for digit in digits.bytes().take_while(u8::is_ascii_digit) {
        number = number.wrapping_mul(10).wrapping_add((digit - b'0') as u64);
    }
    if negative {
        number = number.wrapping_neg();
    }
    Ok(truncated(type_name, number as i128))
}

/// The start of a text that is a decimal number, if it has one.
fn decimal_prefix(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let digits = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count()
    };
    let mut at = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let whole = digits(at);
    at += whole;
    let mut fraction = 0;
    if bytes.get(at) == Some(&b'.') {
        fraction = digits(at + 1);
        if whole + fraction > 0 {
            at += 1 + fraction;
        }
    }
    if whole + fraction == 0 {
        return None;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(bytes.get(at + 1), Some(b'+' | b'-')));
        let exponent = digits(at + 1 + sign);
        if exponent > 0 {
            at += 1 + sign + exponent;
        }
    }
    Some(&text[..at])
}

/// A float as the Editor writes it. Its three names for values that are not
/// finite are read as such. Of any other text the engine reads the decimal
/// number it starts with, and text that starts with none is not a number.
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
    let Some(prefix) = decimal_prefix(text) else {
        // Other spellings of the three names were not compared.
        let name = text.trim_start_matches(['+', '-']).to_ascii_lowercase();
        if (name.starts_with("inf") || name.starts_with("nan"))
            && !matches!(text, "inf" | "-inf" | "nan")
        {
            return Err(mismatch(path, "a name of a number that is not finite"));
        }
        return Ok(f32::NAN.into());
    };
    // What the engine makes of an exponent that breaks off was not compared.
    if matches!(text.as_bytes().get(prefix.len()), Some(b'e' | b'E')) {
        return Err(mismatch(path, "a number with an incomplete exponent"));
    }
    match prefix.parse::<T>() {
        Ok(number) => Ok(number),
        Err(_) => Err(mismatch(path, "text that is not a number")),
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

/// What the engine makes of an engine struct no text sets, where that is
/// known. Two of them are classes, of which it constructs an object: a curve
/// without keys and a gradient from white to white.
fn engine_default(node: &Node) -> Option<Val> {
    Some(match node.type_name.as_str() {
        // A class as well, whose new object was not compared.
        "GUIStyle" => return None,
        "AnimationCurve" => Val::map([
            ("m_PreInfinity", Val::I(2)),
            ("m_PostInfinity", Val::I(2)),
            ("m_RotationOrder", Val::I(4)),
        ]),
        "Gradient" => {
            let white = || Val::map(["r", "g", "b", "a"].map(|part| (part, Val::F(1.0))));
            Val::map([
                ("key0", white()),
                ("key1", white()),
                ("ctime1", Val::U(0xffff)),
                ("atime1", Val::U(0xffff)),
                ("m_ColorSpace", Val::I(-1)),
                ("m_NumColorKeys", Val::U(2)),
                ("m_NumAlphaKeys", Val::U(2)),
            ])
        }
        _ => Val::Zero,
    })
}

impl Encoder<'_> {
    /// One field of a C# type, found by its name or a former name.
    fn field(
        &mut self,
        field: &Field,
        node: &Node,
        mapping: &Map<String, Value>,
        path: &str,
        known: bool,
    ) -> Result<Val> {
        let path = format!("{path}{}", field.name);
        let names = std::iter::once(&field.name).chain(&field.former_names);
        let Some(value) = names.into_iter().find_map(|name| mapping.get(name)) else {
            return self.missing(field, node, &path, known);
        };
        let structure = matches!(value, Value::Object(_) | Value::Array(_));
        match &field.kind {
            // The engine does not read a structure into a number: the field
            // stays as it was. Into a string it reads nothing.
            FieldType::Primitive { .. } | FieldType::Enum { .. } if structure => {
                self.missing(field, node, &path, known)
            }
            FieldType::String if structure => Ok(Val::Str(String::new())),
            // Nor a number or a sequence into a class.
            FieldType::Struct { fields, .. }
                if !fields.is_empty() && matches!(value, Value::String(_) | Value::Array(_)) =>
            {
                self.missing(field, node, &path, known)
            }
            kind => self.value(kind, node, value, &path, known && !field.initial_unknown),
        }
    }

    /// What a field holds that the document does not set: what the
    /// constructor of its class stored in it or, failing that, what the
    /// engine makes of an empty field. `known` tells whether the object the
    /// field belongs to is as its own constructor left it.
    fn missing(&mut self, field: &Field, node: &Node, path: &str, known: bool) -> Result<Val> {
        // The Editor writes nothing for a type without fields.
        if matches!(&field.kind, FieldType::Struct { fields, .. } if fields.is_empty()) {
            return Ok(Val::Zero);
        }
        if !known || field.initial_unknown {
            return Err(unsupported(format!(
                "behaviour field {path} is not in the authored document, and what its class constructs it with is not known"
            )));
        }
        let initial = || Value::String(field.initial.clone().unwrap_or_default());
        Ok(match &field.kind {
            // C# has the sign bit set in its `NaN`, which the text loses.
            FieldType::Primitive { .. } if field.initial.as_deref() == Some("NaN") => {
                match node.type_name.as_str() {
                    "float" => Val::F(-f32::NAN),
                    _ => Val::D(-f64::NAN),
                }
            }
            FieldType::Primitive { .. } | FieldType::Enum { .. } => {
                scalar(&node.type_name, &initial(), path)?
            }
            FieldType::String => Val::Str(field.initial.clone().unwrap_or_default()),
            FieldType::Object { .. } => Val::Zero,
            // What a build makes of a reference the text lacks was not
            // compared.
            FieldType::ManagedReference { .. } => {
                return Err(unsupported(format!(
                    "behaviour field {path} is a managed reference that is not in the authored document"
                )));
            }
            FieldType::Array { .. } => Val::Seq(Vec::new()),
            FieldType::FixedBuffer { length, .. } => Val::Seq(vec![Val::Zero; *length]),
            // The engine constructs the object of an empty field itself.
            FieldType::Struct { fields, .. } => {
                let mut values = Vec::with_capacity(fields.len());
                for (field, child) in fields.iter().zip(&node.children) {
                    values.push((
                        Cow::Owned(field.name.clone()),
                        self.missing(field, child, &format!("{path}.{}", field.name), true)?,
                    ));
                }
                Val::Map(values)
            }
            FieldType::Unity { .. } => match engine_default(node) {
                Some(made) => made,
                None => {
                    return Err(unsupported(format!(
                        "behaviour field {path} is not in the authored document, and what the engine makes of a new {} is not known",
                        node.type_name
                    )));
                }
            },
        })
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

    /// `known` tells whether the object the value is read over is as its
    /// constructor left it, which decides what a field it lacks holds.
    fn value(
        &mut self,
        kind: &FieldType,
        node: &Node,
        value: &Value,
        path: &str,
        known: bool,
    ) -> Result<Val> {
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
                            // The engine reads no element out of a value.
                            return Ok(Val::Seq(Vec::new()));
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
                            // An element is an object of its own.
                            self.value(element, data, item, &format!("{path}[{index}]"), true)
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
                        self.field(field, child, mapping, &format!("{path}."), known)?,
                    ));
                }
                Ok(Val::Map(values))
            }
            FieldType::ManagedReference { .. } => {
                let id = managed_id(value, path)?;
                if !self.held.contains(&id) {
                    self.held.push(id);
                }
                Ok(Val::map([("rid", Val::I(id))]))
            }
            FieldType::Unity { .. } if node.type_name.starts_with("PPtr<") => {
                self.pointer("", "", value, path)
            }
            FieldType::Unity { .. } => self.engine_struct(node, value, path, known),
        }
    }

    /// Appends the registry of the objects held by managed reference, as the
    /// text lists them, and returns the positions of their classes in
    /// `types`. A build was compared for a registry in the Editor's order,
    /// the null entry first and the others by ID, that holds exactly the
    /// objects the fields refer to.
    fn registry(
        &mut self,
        value: &Value,
        types: &[ManagedType],
        out: &mut Vec<u8>,
    ) -> Result<Vec<usize>> {
        let path = REGISTRY_FIELD;
        let Value::Object(registry) = value else {
            return Err(unsupported(
                "behaviour holds managed references and no registry of their objects",
            ));
        };
        if scalar_text(get(registry, "version"), path)? != "2" {
            return Err(unsupported(
                "registry of managed references in another version",
            ));
        }
        let entries: &[Value] = match get(registry, "RefIds") {
            Value::Array(entries) => entries,
            Value::Null => &[],
            Value::String(text) if text.is_empty() => &[],
            _ => return Err(mismatch(path, "a value")),
        };
        out.extend_from_slice(&2i32.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let (mut listed, mut used) = (Vec::new(), Vec::new());
        let empty = Map::new();
        for entry in entries {
            let id = managed_id(entry, path)?;
            let Value::Object(entry) = entry else {
                return Err(mismatch(path, "a value"));
            };
            if listed.last().is_some_and(|last| *last >= id) {
                return Err(unsupported(
                    "registry of managed references that is not in the order of the IDs",
                ));
            }
            listed.push(id);
            let Value::Object(kind) = get(entry, "type") else {
                return Err(mismatch(path, "an object without a class"));
            };
            let name = [
                string(get(kind, "class"), path)?,
                string(get(kind, "ns"), path)?,
                string(get(kind, "asm"), path)?,
            ];
            out.extend_from_slice(&id.to_le_bytes());
            for part in &name {
                out.extend_from_slice(&(part.len() as u32).to_le_bytes());
                out.extend_from_slice(part.as_bytes());
                out.resize(out.len().next_multiple_of(4), 0);
            }
            let [class, namespace, assembly] = &name;
            if id == -2 {
                // The entry of the null reference has neither class nor data.
                if name.iter().any(|part| !part.is_empty()) {
                    return Err(mismatch(path, "a null reference with a class"));
                }
                continue;
            }
            let position = types.iter().position(|kind| {
                kind.class_name == *class
                    && kind.namespace == *namespace
                    && kind.assembly == *assembly
            });
            let Some(position) = position else {
                return Err(unsupported(format!(
                    "managed reference to an object of {class}, a class the layout does not hold"
                )));
            };
            let kind = &types[position];
            let Some((root, tree)) = &kind.data else {
                return Err(unsupported(format!(
                    "managed reference to an object of {class}: {}",
                    kind.unsupported
                )));
            };
            let mapping = match get(entry, "data") {
                Value::Object(mapping) => mapping,
                Value::Null => &empty,
                Value::String(text) if text.is_empty() => &empty,
                _ => return Err(mismatch(path, "an object whose data is a value")),
            };
            let prefix = format!("managedReferences[{id}].");
            let mut values = Vec::with_capacity(kind.fields.len());
            for (field, node) in kind.fields.iter().zip(&root.children) {
                values.push((
                    Cow::Owned(field.name.clone()),
                    self.field(field, node, mapping, &prefix, true)?,
                ));
            }
            out.extend_from_slice(&tree.encode(&Val::Map(values))?);
            if !used.contains(&position) {
                used.push(position);
            }
        }
        out.resize(out.len().next_multiple_of(4), 0);
        self.held.sort_unstable();
        if self.held != listed {
            return Err(unsupported(
                "registry of managed references that does not hold exactly the objects referred to",
            ));
        }
        Ok(used)
    }

    /// A struct the engine serializes itself: the authored keys are the node
    /// names, in the current form of the struct. `known` tells whether the
    /// struct the text is read over is as the engine makes it.
    fn engine_struct(
        &mut self,
        node: &Node,
        value: &Value,
        path: &str,
        known: bool,
    ) -> Result<Val> {
        // Two structs are one scalar in text: a name, and an ID as the 32
        // hex digits of its four words, each from its lowest digit up. A
        // build keeps the hash of a name, as a signed number in digits.
        match (node.type_name.as_str(), node.children.len()) {
            ("string", 1) => {
                let hash = crc32(&string(value, path)?) as i32;
                return Ok(Val::map([("id", Val::Str(hash.to_string()))]));
            }
            ("GUID", _) => {
                let digits = scalar_text(value, path)?;
                let mut words = [0u64; 4];
                for (index, digit) in digits.chars().enumerate() {
                    let (Some(digit), true) = (digit.to_digit(16), digits.len() == 32) else {
                        return Err(mismatch(path, "text that is not an ID"));
                    };
                    words[index / 8] |= (digit as u64) << (4 * (index % 8));
                }
                if digits.len() != 32 {
                    return Err(mismatch(path, "text that is not an ID"));
                }
                return Ok(Val::Map(
                    node.children
                        .iter()
                        .zip(words)
                        .map(|(child, word)| (Cow::Owned(child.name.clone()), Val::U(word)))
                        .collect(),
                ));
            }
            _ => {}
        }
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
                // A part the text lacks stays as the engine made it, unless
                // the constructor of the class set the struct. Of the
                // structs that have had several forms, this was compared for
                // a rectangle and a curve.
                let compared =
                    expected <= 1 || matches!(node.type_name.as_str(), "Rectf" | "AnimationCurve");
                if known
                    && compared
                    && child.version <= 1
                    && let Some(made) = engine_default(node)
                {
                    let made = match made {
                        Val::Map(parts) => parts
                            .into_iter()
                            .find(|(name, _)| *name == child.name)
                            .map(|(_, part)| part),
                        _ => None,
                    };
                    values.push((Cow::Owned(child.name.clone()), made.unwrap_or(Val::Zero)));
                    continue;
                }
                return Err(unsupported(format!(
                    "behaviour field {path} is not in the authored document"
                )));
            };
            let value = if child.type_name == "bool" {
                // What the engine reads out of other text was not compared.
                match scalar_text(authored, &path)? {
                    "0" => Val::Bool(false),
                    "1" => Val::Bool(true),
                    _ => return Err(mismatch(&path, "text that is not a switch")),
                }
            } else if child.children.is_empty() {
                scalar(&child.type_name, authored, &path)?
            } else if child.type_name == "string" {
                Val::Str(string(authored, &path)?)
            } else if let Some(class) = child
                .type_name
                .strip_prefix("PPtr<")
                .and_then(|class| class.strip_suffix('>'))
            {
                self.pointer(class, "UnityEngine", authored, &path)?
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
                            self.engine_struct(
                                child.element(),
                                item,
                                &format!("{path}[{index}]"),
                                true,
                            )
                        })
                        .collect::<Result<_>>()?,
                )
            } else {
                self.engine_struct(child, authored, &path, known)?
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
