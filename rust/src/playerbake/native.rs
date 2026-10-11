//! Native-format assets the Player holds as their authored fields. Each
//! object is written by walking the Player type tree of its class and taking
//! every field from the source by name. A source field the Player lacks is
//! Editor-only and dropped, as the engine drops it; a Player field the source
//! lacks would take a default only the engine knows, so it is refused. What
//! the engine corrects after reading a source is restated per class.
use super::{
    Baked,
    serialized::{self, External, Object},
    source::{get, text},
    typetree::{TypeTree, Val},
    unsupported,
};
use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::sync::LazyLock;

/// What a native-format asset takes from its project beyond its own text.
/// The classes baked so far take nothing.
pub trait NativeSources {}

/// For a caller without a project.
pub struct NoNativeSources;
impl NativeSources for NoNativeSources {}

struct Class {
    id: i32,
    /// The managed type a session publishes the object as.
    managed: &'static str,
    tree: LazyLock<Result<TypeTree, String>>,
    /// Restates what the engine corrects after it has read the source.
    settle: fn(&mut Val) -> Result<()>,
}

macro_rules! class {
    ($id:literal, $managed:literal, $settle:expr) => {
        Class {
            id: $id,
            managed: $managed,
            tree: LazyLock::new(|| {
                TypeTree::parse(&include_bytes!(concat!("engine/type-", $id, ".bin"))[..])
                    .map_err(|error| error.to_string())
            }),
            settle: $settle,
        }
    };
}

static CLASSES: [Class; 1] = [class!(319, "UnityEngine.AvatarMask", as_authored)];

/// For a class the engine reads without correcting anything.
fn as_authored(_: &mut Val) -> Result<()> {
    Ok(())
}

fn class(id: i64) -> Option<&'static Class> {
    CLASSES.iter().find(|class| class.id as i64 == id)
}

/// Whether `bake_native` bakes objects of a class.
pub fn native_class(class_id: i64) -> bool {
    class(class_id).is_some()
}

/// The Player layout of a class `bake_native` bakes, in the pinned engine.
pub fn native_tree(class_id: i64) -> Result<&'static TypeTree> {
    let Some(class) = class(class_id) else {
        bail!("class {class_id} is not a baked native class");
    };
    class
        .tree
        .as_ref()
        .map_err(|error| anyhow::anyhow!("pinned type tree of class {class_id}: {error}"))
}

/// The objects a source can refer to and the other sources it has named.
struct References<'a> {
    locals: &'a [i64],
    externals: Vec<External>,
}

impl References<'_> {
    fn pointer(&mut self, value: &Value, path: &str) -> Result<Val> {
        let map = match value {
            Value::Null => return Ok(Val::Zero),
            Value::Object(map) => map,
            _ => bail!("{path} is not an object reference"),
        };
        let file_id = text(get(map, "fileID"))?;
        let Ok(path_id) = file_id.parse::<i64>() else {
            bail!("{path} has an invalid file ID");
        };
        if path_id == 0 {
            return Ok(Val::Zero);
        }
        let guid = text(get(map, "guid"))?.to_ascii_lowercase();
        let file = if guid.is_empty() {
            if !self.locals.contains(&path_id) {
                return Err(unsupported(format!(
                    "{path} refers to an object its source does not hold"
                )));
            }
            0
        } else {
            match self
                .externals
                .iter()
                .position(|external| external.guid == guid)
            {
                Some(position) => position + 1,
                None => {
                    self.externals.push(External { guid });
                    self.externals.len()
                }
            }
        };
        Ok(Val::map([
            ("m_FileID", Val::I(file as i64)),
            ("m_PathID", Val::I(path_id)),
        ]))
    }
}

/// A decimal number as the Editor writes one. Other spellings, which the
/// engine may read differently, are refused.
fn decimal(text: &str, fraction: bool) -> bool {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let text = text.strip_prefix('-').unwrap_or(text);
    if !fraction {
        return digits(text);
    }
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (text, None),
    };
    let whole = match mantissa.split_once('.') {
        Some((whole, part)) => digits(part).then_some(whole),
        None => Some(mantissa),
    };
    whole.is_some_and(digits)
        && exponent
            .is_none_or(|exponent| digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)))
}

fn scalar(type_name: &str, source: &str, path: &str) -> Result<Val> {
    let refused = || unsupported(format!("{path} is not a plain number: {source:?}"));
    Ok(match type_name {
        "float" => {
            let number = source
                .parse::<f32>()
                .ok()
                .filter(|number| number.is_finite());
            match number {
                Some(number) if decimal(source, true) => Val::F(number),
                _ => return Err(refused()),
            }
        }
        "double" => {
            let number = source
                .parse::<f64>()
                .ok()
                .filter(|number| number.is_finite());
            match number {
                Some(number) if decimal(source, true) => Val::D(number),
                _ => return Err(refused()),
            }
        }
        "bool" => match source {
            "0" => Val::Bool(false),
            "1" => Val::Bool(true),
            _ => return Err(refused()),
        },
        "UInt8" | "UInt16" | "unsigned short" | "unsigned int" | "UInt64"
        | "unsigned long long" => match source.parse::<u64>() {
            Ok(number) if decimal(source, false) => Val::U(number),
            _ => return Err(refused()),
        },
        "SInt8" | "SInt16" | "short" | "int" | "SInt64" | "long long" => {
            match source.parse::<i64>() {
                Ok(number) if decimal(source, false) => Val::I(number),
                _ => return Err(refused()),
            }
        }
        other => return Err(unsupported(format!("{path} has the type {other}"))),
    })
}

/// The Editor writes a sequence of integers as one run of hexadecimal
/// digits, each item in little-endian byte order; a sequence of unsigned
/// 32-bit integers ends with `u`.
fn hex_items(type_name: &str, dump: &str, path: &str) -> Result<Vec<Val>> {
    let (size, signed, suffix) = match type_name {
        "UInt8" => (1, false, ""),
        "SInt8" => (1, true, ""),
        "UInt16" | "unsigned short" => (2, false, ""),
        "SInt16" | "short" => (2, true, ""),
        "unsigned int" => (4, false, "u"),
        "int" => (4, true, ""),
        other => {
            return Err(unsupported(format!(
                "{path} is a hexadecimal run of {other} items"
            )));
        }
    };
    let digits = dump.strip_suffix(suffix).unwrap_or(dump).as_bytes();
    let (items, rest) = digits.as_chunks::<2>();
    if !rest.is_empty()
        || !items.len().is_multiple_of(size)
        || !digits.iter().all(u8::is_ascii_hexdigit)
    {
        return Err(unsupported(format!(
            "{path} is not a hexadecimal run of {type_name} items"
        )));
    }
    let bytes: Vec<u8> = items
        .iter()
        .map(|pair| u8::from_str_radix(&String::from_utf8_lossy(pair), 16).unwrap_or_default())
        .collect();
    Ok(bytes
        .chunks(size)
        .map(|item| {
            let mut wide = [0u8; 8];
            wide[..size].copy_from_slice(item);
            let number = u64::from_le_bytes(wide);
            if signed {
                let shift = 64 - 8 * size as u32;
                Val::I(((number << shift) as i64) >> shift)
            } else {
                Val::U(number)
            }
        })
        .collect())
}

struct Transcoder<'a> {
    tree: &'a TypeTree,
    references: References<'a>,
}

impl Transcoder<'_> {
    fn value(&mut self, index: usize, source: &Value, path: &str) -> Result<Val> {
        let node = &self.tree.nodes[index];
        let children = self.tree.children(index);
        if children.is_empty() {
            if node.size == 0 {
                return Ok(Val::Zero);
            }
            return match source {
                Value::String(source) => scalar(&node.type_name, source, path),
                _ => bail!("{path} is not a scalar"),
            };
        }
        if node.type_name == "string" {
            return match source {
                Value::Null => Ok(Val::Str(String::new())),
                Value::String(source) => Ok(Val::Str(source.clone())),
                _ => bail!("{path} is not a string"),
            };
        }
        if node.type_name.starts_with("PPtr<") {
            return self.references.pointer(source, path);
        }
        if let Some((_, element)) = self.tree.element(index) {
            return self.sequence(index, element, source, path);
        }
        let Value::Object(fields) = source else {
            return Err(unsupported(format!(
                "{path} is not written as a mapping of its fields"
            )));
        };
        self.structure(index, fields, path)
    }

    fn sequence(
        &mut self,
        index: usize,
        element: usize,
        source: &Value,
        path: &str,
    ) -> Result<Val> {
        let node = &self.tree.nodes[index];
        let item = &self.tree.nodes[element];
        if node.type_name != "vector" {
            // A map or set is stored in an order the engine derives.
            return Err(unsupported(format!(
                "{path} is a {} container",
                node.type_name
            )));
        }
        Ok(Val::Seq(match source {
            Value::Null => Vec::new(),
            Value::String(dump) if self.tree.children(element).is_empty() => {
                hex_items(&item.type_name, dump, path)?
            }
            Value::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for (position, item) in items.iter().enumerate() {
                    values.push(self.value(element, item, &format!("{path}[{position}]"))?);
                }
                values
            }
            _ => bail!("{path} is not a sequence"),
        }))
    }

    fn structure(&mut self, index: usize, fields: &Map<String, Value>, path: &str) -> Result<Val> {
        // A source of another version lays its fields out differently and
        // the engine converts it on load.
        let version = match fields.get("serializedVersion") {
            None => 1,
            Some(version) => text(version)?.parse::<u16>().unwrap_or(0),
        };
        let node = &self.tree.nodes[index];
        if version != node.version {
            return Err(unsupported(format!(
                "{} {path} has serialized version {version}, the engine writes {}",
                node.type_name, node.version
            )));
        }
        let mut values = Vec::new();
        for child in self.tree.children(index) {
            let name = &self.tree.nodes[child].name;
            let path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}.{name}")
            };
            let Some(field) = fields.get(name) else {
                return Err(unsupported(format!("the source has no {path}")));
            };
            values.push((
                std::borrow::Cow::Owned(name.clone()),
                self.value(child, field, &path)?,
            ));
        }
        Ok(Val::Map(values))
    }
}

/// One object of a baked native source.
pub struct NativeObject {
    pub file_id: String,
    pub class_id: i32,
    pub data: Vec<u8>,
}

/// The serialized objects of a source, in its order, and the sources they
/// refer to in order of first use.
pub fn bake_native_objects(
    yaml: &[u8],
    _sources: &dyn NativeSources,
) -> Result<(Vec<NativeObject>, Vec<External>)> {
    let documents = crate::yaml::parse_unity_yaml_lexical(yaml)?;
    let mut locals = Vec::new();
    for document in &documents {
        if document.stripped || class(document.class_id).is_none() {
            return Err(unsupported(format!(
                "native source with an object of class {} ({})",
                document.class_id, document.type_name
            )));
        }
        let Ok(local_id) = document.file_id.parse::<i64>() else {
            bail!("object has an invalid file ID");
        };
        locals.push(local_id);
    }
    let mut references = References {
        locals: &locals,
        externals: Vec::new(),
    };
    let mut objects = Vec::new();
    for document in &documents {
        let class = class(document.class_id).unwrap();
        let tree = native_tree(document.class_id)?;
        let mut transcoder = Transcoder { tree, references };
        let mut value = transcoder.structure(0, &document.data, "")?;
        references = transcoder.references;
        (class.settle)(&mut value)?;
        objects.push(NativeObject {
            file_id: document.file_id.clone(),
            class_id: class.id,
            data: tree.encode(&value)?,
        });
    }
    Ok((objects, references.externals))
}

/// Bakes an authored native-format source into a ContentFile. Every object
/// is published under its authored local file ID.
pub fn bake_native(yaml: &[u8], sources: &dyn NativeSources) -> Result<Baked> {
    let (baked, externals) = bake_native_objects(yaml, sources)?;
    let mut objects = Vec::new();
    let mut published = Vec::new();
    for object in baked {
        let class = class(object.class_id as i64).unwrap();
        objects.push(Object {
            local_id: object.file_id.parse()?,
            tree: native_tree(object.class_id as i64)?,
            data: object.data,
            script: None,
        });
        published.push((object.file_id, class.managed.to_string()));
    }
    Ok(Baked {
        file: serialized::write(&objects, &externals)?,
        objects: published,
        externals,
    })
}
