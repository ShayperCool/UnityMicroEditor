//! Player type trees of the pinned engine and a value encoder driven by them.
//! The encoder writes exactly the fields a tree declares, in its order and
//! with its alignment, so a layout is never restated in code.
use anyhow::{Result, bail};
use std::borrow::Cow;

/// Unity's common type-tree string table, as stored in the pinned Player.
static COMMON_STRINGS: &[u8] = include_bytes!("engine/common-strings.bin");

#[derive(Debug, Clone)]
pub struct Node {
    pub type_name: String,
    pub name: String,
    pub level: u8,
    pub array: bool,
    pub align: bool,
    /// The byte size the tree declares; negative when it varies.
    pub size: i32,
    /// The serialized version of the type, which a source states for its
    /// own layout when it is not 1.
    pub version: u16,
    /// The node's type flags (1: array, 2: held by managed reference) and
    /// meta flags (0x4000: aligned after, 0x8000: ...), as stored.
    pub type_flags: u8,
    pub meta: u32,
}

/// One class of a SerializedFile type table: the verbatim entry and its nodes.
pub struct TypeTree {
    pub class_id: i32,
    /// The complete type table entry, written unchanged into a baked file.
    pub entry: Cow<'static, [u8]>,
    pub nodes: Vec<Node>,
}

/// `MonoBehaviour`, the class of every script-backed object.
pub(super) const SCRIPT_CLASS: i32 = 114;

/// The offset of a name in the engine's common strings, if it is one of them.
fn common_offset(text: &str) -> Option<u32> {
    static OFFSETS: std::sync::OnceLock<std::collections::HashMap<&'static [u8], u32>> =
        std::sync::OnceLock::new();
    let offsets = OFFSETS.get_or_init(|| {
        let mut offsets = std::collections::HashMap::new();
        let mut at = 0;
        for part in COMMON_STRINGS.split(|&byte| byte == 0) {
            if !part.is_empty() {
                offsets.entry(part).or_insert(at as u32);
            }
            at += part.len() + 1;
        }
        offsets
    });
    offsets.get(text.as_bytes()).copied()
}

fn le32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn string(table: &[u8], offset: usize) -> Result<String> {
    let Some(rest) = table.get(offset..) else {
        bail!("type tree string offset exceeds its table");
    };
    let Some(end) = rest.iter().position(|&byte| byte == 0) else {
        bail!("type tree string is unterminated");
    };
    Ok(String::from_utf8_lossy(&rest[..end]).into_owned())
}

impl TypeTree {
    /// Parses an entry laid out as class ID, flags, hashes, blob and
    /// dependency count. The entry of a script class also carries the hash
    /// that identifies the script.
    pub fn parse(entry: impl Into<Cow<'static, [u8]>>) -> Result<Self> {
        let owned: Cow<'static, [u8]> = entry.into();
        let entry: &[u8] = &owned;
        if entry.len() < 4 + 3 + 48 + 4 + 16 {
            bail!("type table entry is truncated");
        }
        let class_id = le32(entry, 0) as i32;
        let at = if class_id == SCRIPT_CLASS { 55 } else { 39 };
        let size = le32(entry, at) as usize;
        let Some(blob) = entry.get(at + 4..at + 4 + size) else {
            bail!("type tree blob exceeds its entry");
        };
        // The entry ends in the types its objects hold by managed reference,
        // as positions in a table of the file.
        let tail = at + 4 + size;
        let Some(held) = entry.get(tail..tail + 4) else {
            bail!("type table entry is truncated");
        };
        let held = u32::from_le_bytes([held[0], held[1], held[2], held[3]]) as usize;
        if entry.len() != tail + 4 + 4 * held {
            bail!("type table entry bounds are invalid");
        }
        if size < 16 || le32(blob, 0) != 0x7474686d || le32(blob, 4) != 23 {
            bail!("unsupported type tree format");
        }
        let count = le32(blob, 8) as usize;
        let chars = le32(blob, 12) as usize;
        if count == 0 || 16 + count * 32 + chars != blob.len() {
            bail!("type tree node/string bounds are invalid");
        }
        let local = &blob[16 + count * 32..];
        let name = |offset: u32| {
            if offset & 0x8000_0000 != 0 {
                string(COMMON_STRINGS, (offset & 0x7fff_ffff) as usize)
            } else {
                string(local, offset as usize)
            }
        };
        let mut nodes = Vec::with_capacity(count);
        for index in 0..count {
            let node = &blob[16 + index * 32..16 + index * 32 + 32];
            nodes.push(Node {
                type_name: name(le32(node, 4))?,
                name: name(le32(node, 8))?,
                level: node[2],
                array: node[3] & 1 != 0,
                align: le32(node, 20) & 0x4000 != 0,
                size: le32(node, 12) as i32,
                version: u16::from_le_bytes([node[0], node[1]]),
                type_flags: node[3],
                meta: le32(node, 20),
            });
        }
        if nodes[0].level != 0 {
            bail!("type tree root is not at level zero");
        }
        Ok(Self {
            class_id,
            entry: owned,
            nodes,
        })
    }

    /// The entry of a tree laid out by the tool, as the engine writes one: the
    /// nodes in order, each name in the engine's common strings by its offset
    /// there and otherwise in the tree's own table at its first use. A script
    /// class takes the hash of its script, every class the hash of its tree;
    /// the second hash format 23 has is that of the blob.
    pub fn of_nodes(
        class_id: i32,
        script_hash: Option<[u8; 16]>,
        type_hash: [u8; 16],
        nodes: Vec<Node>,
    ) -> Result<Self> {
        if nodes.first().is_none_or(|root| root.level != 0) {
            bail!("type tree root is not at level zero");
        }
        if (class_id == SCRIPT_CLASS) != script_hash.is_some() {
            bail!("only a script class has a script hash");
        }
        let mut local: Vec<u8> = Vec::new();
        let mut offsets: std::collections::HashMap<String, u32> = Default::default();
        let mut offset = |text: &str| -> u32 {
            if let Some(at) = common_offset(text) {
                return 0x8000_0000 | at;
            }
            *offsets.entry(text.to_string()).or_insert_with(|| {
                let at = local.len() as u32;
                local.extend_from_slice(text.as_bytes());
                local.push(0);
                at
            })
        };
        let mut table = Vec::with_capacity(nodes.len() * 32);
        for (index, node) in nodes.iter().enumerate() {
            table.extend_from_slice(&node.version.to_le_bytes());
            table.push(node.level);
            table.push(node.type_flags);
            table.extend_from_slice(&offset(&node.type_name).to_le_bytes());
            table.extend_from_slice(&offset(&node.name).to_le_bytes());
            table.extend_from_slice(&node.size.to_le_bytes());
            table.extend_from_slice(&(index as u32).to_le_bytes());
            table.extend_from_slice(&node.meta.to_le_bytes());
            table.extend_from_slice(&0u64.to_le_bytes());
        }
        let mut blob = Vec::with_capacity(16 + table.len() + local.len());
        blob.extend_from_slice(&0x7474_686d_u32.to_le_bytes());
        blob.extend_from_slice(&23u32.to_le_bytes());
        blob.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
        blob.extend_from_slice(&(local.len() as u32).to_le_bytes());
        blob.extend_from_slice(&table);
        blob.extend_from_slice(&local);
        let mut entry = Vec::with_capacity(blob.len() + 64);
        entry.extend_from_slice(&class_id.to_le_bytes());
        entry.push(0);
        entry.extend_from_slice(&(if script_hash.is_some() { 0i16 } else { -1 }).to_le_bytes());
        if let Some(hash) = script_hash {
            entry.extend_from_slice(&hash);
        }
        entry.extend_from_slice(&type_hash);
        entry.extend_from_slice(&xxhash_rust::xxh3::xxh3_128(&blob[16..]).to_le_bytes());
        entry.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        entry.extend_from_slice(&blob);
        entry.extend_from_slice(&0u32.to_le_bytes());
        Self::parse(entry)
    }

    fn end(&self, index: usize) -> usize {
        let mut next = index + 1;
        while next < self.nodes.len() && self.nodes[next].level > self.nodes[index].level {
            next += 1;
        }
        next
    }

    pub fn children(&self, index: usize) -> Vec<usize> {
        let end = self.end(index);
        let mut result = Vec::new();
        let mut next = index + 1;
        while next < end {
            result.push(next);
            next = self.end(next);
        }
        result
    }

    /// The element node of a container (`vector`, `map`, `string`, ...), if
    /// this node is one.
    pub fn element(&self, index: usize) -> Option<(usize, usize)> {
        let children = self.children(index);
        let array = *children.first()?;
        if children.len() != 1 || !self.nodes[array].array {
            return None;
        }
        let parts = self.children(array);
        (parts.len() == 2).then(|| (array, parts[1]))
    }

    pub fn encode(&self, value: &Val) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.write(0, value, &mut out)?;
        Ok(out)
    }

    /// Reads an object written with this tree back into a value. Scalars come
    /// back as `U`, `I`, `F`, `D` or `Bool` by their declared type.
    pub fn decode(&self, data: &[u8]) -> Result<Val> {
        let mut at = 0;
        let value = self.read(0, data, &mut at)?;
        if at != data.len() {
            bail!(
                "object has {} bytes its type tree does not describe",
                data.len() - at
            );
        }
        Ok(value)
    }

    /// Reads the first fields of an object and leaves the rest, for an object
    /// whose tail is too large to be worth a value.
    pub fn decode_leading(&self, data: &[u8], fields: usize) -> Result<Val> {
        let mut at = 0;
        let mut values = Vec::new();
        for child in self.children(0).into_iter().take(fields) {
            let name = Cow::Owned(self.nodes[child].name.clone());
            values.push((name, self.read(child, data, &mut at)?));
        }
        Ok(Val::Map(values))
    }

    fn read(&self, index: usize, data: &[u8], at: &mut usize) -> Result<Val> {
        let node = &self.nodes[index];
        let mut take = |size: usize| -> Result<&[u8]> {
            let Some(bytes) = data.get(*at..*at + size) else {
                bail!("object ends inside {} {}", node.type_name, node.name);
            };
            *at += size;
            Ok(bytes)
        };
        let children = self.children(index);
        let value = if children.is_empty() && node.size == 0 {
            // A structure without fields.
            Val::Zero
        } else if children.is_empty() {
            match node.type_name.as_str() {
                "bool" => Val::Bool(take(1)?[0] != 0),
                "UInt8" => Val::U(take(1)?[0] as u64),
                "SInt8" | "char" => Val::I(take(1)?[0] as i8 as i64),
                "UInt16" | "unsigned short" => {
                    Val::U(u16::from_le_bytes(take(2)?.try_into()?) as u64)
                }
                "SInt16" | "short" => Val::I(i16::from_le_bytes(take(2)?.try_into()?) as i64),
                "unsigned int" => Val::U(u32::from_le_bytes(take(4)?.try_into()?) as u64),
                "int" | "Type*" => Val::I(i32::from_le_bytes(take(4)?.try_into()?) as i64),
                "UInt64" | "unsigned long long" | "FileSize" => {
                    Val::U(u64::from_le_bytes(take(8)?.try_into()?))
                }
                "SInt64" | "long long" => Val::I(i64::from_le_bytes(take(8)?.try_into()?)),
                "float" => Val::F(f32::from_le_bytes(take(4)?.try_into()?)),
                "double" => Val::D(f64::from_le_bytes(take(8)?.try_into()?)),
                other => bail!("unsupported scalar type {other}"),
            }
        } else if node.type_name == "TypelessData" {
            // A byte count and that many bytes, which the tree lists as two
            // fields of the node.
            let count = u32::from_le_bytes(take(4)?.try_into()?) as usize;
            Val::Bytes(take(count)?.to_vec())
        } else if let Some((array, element)) = self.element(index) {
            let count = u32::from_le_bytes(take(4)?.try_into()?) as usize;
            let value = if node.type_name == "string" {
                // A string that is not text keeps its bytes.
                let bytes = take(count)?;
                match std::str::from_utf8(bytes) {
                    Ok(text) => Val::Str(text.to_string()),
                    Err(_) => Val::Bytes(bytes.to_vec()),
                }
            } else {
                if count > data.len() {
                    bail!("sequence length exceeds its object");
                }
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(self.read(element, data, at)?);
                }
                Val::Seq(items)
            };
            if self.nodes[array].align {
                *at = (*at + 3) & !3;
            }
            value
        } else if node.type_name == "OffsetPtr" && children.len() == 1 {
            self.read(children[0], data, at)?
        } else {
            let mut fields = Vec::with_capacity(children.len());
            for &child in &children {
                fields.push((
                    Cow::Owned(self.nodes[child].name.clone()),
                    self.read(child, data, at)?,
                ));
            }
            Val::Map(fields)
        };
        if node.align {
            *at = (*at + 3) & !3;
        }
        Ok(value)
    }

    fn write(&self, index: usize, value: &Val, out: &mut Vec<u8>) -> Result<()> {
        let node = &self.nodes[index];
        let fail = |what: &str| -> anyhow::Error {
            anyhow::anyhow!(
                "{what} for {} {} of class {}",
                node.type_name,
                node.name,
                self.class_id
            )
        };
        let children = self.children(index);
        if children.is_empty() && node.size == 0 {
            // A structure without fields takes no bytes.
        } else if children.is_empty() {
            scalar(&node.type_name, value, out).map_err(|error| fail(&error.to_string()))?;
        } else if node.type_name == "TypelessData" {
            match value {
                Val::Zero => out.extend_from_slice(&0u32.to_le_bytes()),
                Val::Bytes(bytes) => {
                    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                    out.extend_from_slice(bytes);
                }
                _ => return Err(fail("value is not a run of bytes")),
            }
        } else if let Some((array, element)) = self.element(index) {
            match value {
                Val::Zero => out.extend_from_slice(&0u32.to_le_bytes()),
                Val::Str(text) if node.type_name == "string" => {
                    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
                    out.extend_from_slice(text.as_bytes());
                }
                Val::Seq(items) if node.type_name != "string" => {
                    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
                    for item in items {
                        self.write(element, item, out)?;
                    }
                }
                // Bytes stand for a string that need not be text and for a
                // sequence of one-byte numbers.
                Val::Bytes(bytes)
                    if self.children(element).is_empty() && self.nodes[element].size == 1 =>
                {
                    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                    out.extend_from_slice(bytes);
                }
                _ => return Err(fail("value is not a sequence")),
            }
            if self.nodes[array].align {
                align(out);
            }
        } else if node.type_name == "OffsetPtr" && children.len() == 1 {
            // A pointer in memory; serialized as the pointee itself.
            self.write(children[0], value, out)?;
        } else {
            let fields: &[(Cow<'static, str>, Val)] = match value {
                Val::Zero => &[],
                Val::Map(fields) => fields,
                _ => return Err(fail("value is not a structure")),
            };
            for (name, _) in fields {
                if !children
                    .iter()
                    .any(|&child| self.nodes[child].name == *name)
                {
                    return Err(fail(&format!("unknown field {name}")));
                }
            }
            for &child in &children {
                let name = &self.nodes[child].name;
                let field = fields
                    .iter()
                    .find(|(key, _)| key == name)
                    .map_or(&Val::Zero, |(_, value)| value);
                self.write(child, field, out)?;
            }
        }
        if node.align {
            align(out);
        }
        Ok(())
    }
}

fn align(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

fn scalar(type_name: &str, value: &Val, out: &mut Vec<u8>) -> Result<()> {
    let (size, signed) = match type_name {
        "bool" | "UInt8" => (1, false),
        "SInt8" | "char" => (1, true),
        "UInt16" | "unsigned short" => (2, false),
        "SInt16" | "short" => (2, true),
        "unsigned int" => (4, false),
        // `Type*` holds a class ID.
        "int" | "Type*" => (4, true),
        "UInt64" | "unsigned long long" | "FileSize" => (8, false),
        "SInt64" | "long long" => (8, true),
        "float" => {
            let number = match value {
                Val::Zero => 0.0,
                Val::F(number) => *number,
                _ => bail!("value is not a float"),
            };
            out.extend_from_slice(&number.to_le_bytes());
            return Ok(());
        }
        "double" => {
            let number = match value {
                Val::Zero => 0.0,
                Val::D(number) => *number,
                _ => bail!("value is not a double"),
            };
            out.extend_from_slice(&number.to_le_bytes());
            return Ok(());
        }
        _ => bail!("unsupported scalar type"),
    };
    let number: i128 = match value {
        Val::Zero => 0,
        Val::Bool(flag) => *flag as i128,
        Val::I(number) => *number as i128,
        Val::U(number) => *number as i128,
        _ => bail!("value is not an integer"),
    };
    let bits = size * 8;
    let (low, high) = if signed {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    };
    if number < low || number > high {
        bail!("integer exceeds its field");
    }
    out.extend_from_slice(&(number as i64).to_le_bytes()[..size]);
    Ok(())
}

/// A value to serialize. `Zero` is the all-zero default of any node: zero
/// scalars, empty containers and structures of such.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Zero,
    Bool(bool),
    I(i64),
    U(u64),
    F(f32),
    D(f64),
    Str(String),
    /// The elements of a string or of a sequence of one-byte numbers.
    Bytes(Vec<u8>),
    Seq(Vec<Val>),
    Map(Vec<(Cow<'static, str>, Val)>),
}

impl Val {
    /// A field of a structure value.
    pub fn field(&self, name: &str) -> Option<&Val> {
        match self {
            Val::Map(fields) => fields.iter().find(|(key, _)| key == name).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn field_mut(&mut self, name: &str) -> Option<&mut Val> {
        match self {
            Val::Map(fields) => fields
                .iter_mut()
                .find(|(key, _)| key == name)
                .map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn map<const N: usize>(fields: [(&'static str, Val); N]) -> Val {
        Val::Map(
            fields
                .into_iter()
                .map(|(name, value)| (Cow::Borrowed(name), value))
                .collect(),
        )
    }
    pub fn floats(values: impl IntoIterator<Item = f32>) -> Val {
        Val::Seq(values.into_iter().map(Val::F).collect())
    }
    pub fn str(text: &str) -> Val {
        Val::Str(text.to_string())
    }
}
