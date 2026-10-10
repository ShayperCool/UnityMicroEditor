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
}

/// One class of a SerializedFile type table: the verbatim entry and its nodes.
pub struct TypeTree {
    pub class_id: i32,
    /// The complete type table entry, written unchanged into a baked file.
    pub entry: &'static [u8],
    pub nodes: Vec<Node>,
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
    /// dependency count.
    pub fn parse(entry: &'static [u8]) -> Result<Self> {
        if entry.len() < 4 + 3 + 32 + 4 + 16 {
            bail!("type table entry is truncated");
        }
        let class_id = le32(entry, 0) as i32;
        let size = le32(entry, 39) as usize;
        let Some(blob) = entry.get(43..43 + size) else {
            bail!("type tree blob exceeds its entry");
        };
        if entry.len() != 43 + size + 4 || le32(entry, 43 + size) != 0 {
            bail!("type table entry has unsupported dependencies");
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
            });
        }
        if nodes[0].level != 0 {
            bail!("type tree root is not at level zero");
        }
        Ok(Self {
            class_id,
            entry,
            nodes,
        })
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
        if children.is_empty() {
            scalar(&node.type_name, value, out).map_err(|error| fail(&error.to_string()))?;
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
    Str(String),
    Seq(Vec<Val>),
    Map(Vec<(Cow<'static, str>, Val)>),
}

impl Val {
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
