use crate::project::strconv::{parse_float, parse_int, parse_uint};
use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::{collections::HashSet, sync::LazyLock};
use yaml_rust2::{
    parser::{Event, Parser, Tag},
    scanner::TScalarStyle,
};

pub const MAX_TEXT_BYTES: usize = 32 << 20;
static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^--- !u!([0-9]+) &(-?[0-9]+)(?: (stripped))?[ \t]*\r?$").unwrap()
});
static GUID: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"(?m)^guid:[ \t]*([0-9a-fA-F]{32})[ \t]*\r?$").unwrap()
});

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    pub class_id: i64,
    pub file_id: String,
    pub type_name: String,
    pub data: Map<String, Value>,
    pub stripped: bool,
}

/// Unity IDs and native hex buffers are lexical strings, even when unquoted.
pub fn parse_unity_yaml(content: &[u8]) -> Result<Vec<Document>> {
    parse_documents(content, false)
}

/// Reads documents with every scalar kept as its source text, for consumers
/// that convert by the target field's type: `-0`, a float's exact digits and
/// a name that looks like a number all survive.
pub fn parse_unity_yaml_lexical(content: &[u8]) -> Result<Vec<Document>> {
    parse_documents(content, true)
}

fn parse_documents(content: &[u8], lexical: bool) -> Result<Vec<Document>> {
    ensure!(
        content.len() <= MAX_TEXT_BYTES,
        "unsupported binary or oversized Unity text asset"
    );
    let Ok(text) = std::str::from_utf8(content) else {
        bail!("unsupported binary or oversized Unity text asset")
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let headers: Vec<_> = HEADER.captures_iter(text).collect();
    ensure!(
        !headers.is_empty(),
        "Unity text asset has no document headers"
    );
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(headers.len());
    for (i, header) in headers.iter().enumerate() {
        let class_id = crate::project::strconv::atoi(&header[1])?;
        let id = &header[2];
        ensure!(parse_int(id, 10).is_ok(), "invalid fileID {id}");
        ensure!(seen.insert(id), "duplicate fileID {id}");
        let start = header.get(0).unwrap().end();
        let end = headers
            .get(i + 1)
            .map_or(text.len(), |h| h.get(0).unwrap().start());
        let value = match parse_yaml(&text[start..end], lexical) {
            Err(error) if error.is::<EmptyDocument>() => bail!("fileID {id} requires one object"),
            other => other.with_context(|| format!("fileID {id}"))?,
        };
        let Value::Object(root) = value else {
            bail!("fileID {id} requires one object mapping")
        };
        ensure!(root.len() == 1, "fileID {id} requires one object mapping");
        let (name, value) = root.into_iter().next().unwrap();
        let Value::Object(data) = value else {
            bail!("fileID {id} fields are not a mapping")
        };
        result.push(Document {
            class_id,
            file_id: id.into(),
            type_name: name,
            data,
            stripped: header.get(3).is_some(),
        });
    }
    Ok(result)
}

pub fn metadata_guid(data: &[u8]) -> Option<String> {
    GUID.captures(data)
        .map(|m| String::from_utf8_lossy(&m[1]).to_ascii_lowercase())
}

enum Container {
    Mapping(Map<String, Value>, Option<String>),
    Sequence(Vec<Value>),
}
#[derive(Default)]
struct Receiver {
    stack: Vec<Container>,
    root: Option<Value>,
    /// Keep every scalar as the text Unity wrote.
    lexical: bool,
}
impl Receiver {
    fn insert(&mut self, value: Value) -> Result<()> {
        match self.stack.last_mut() {
            Some(Container::Sequence(items)) => items.push(value),
            Some(Container::Mapping(map, pending)) => {
                let Some(key) = pending.take() else {
                    bail!("non-scalar YAML key")
                };
                ensure!(!map.contains_key(&key), "duplicate YAML key {key:?}");
                map.insert(key, value);
            }
            None => {
                ensure!(self.root.is_none(), "requires one YAML object");
                self.root = Some(value);
            }
        }
        Ok(())
    }
    fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Alias(_) => bail!("YAML aliases are unsupported"),
            Event::MappingStart(_, _) | Event::SequenceStart(_, _) => {
                ensure!(self.stack.len() <= 128, "Unity YAML nesting exceeds 128");
                ensure!(
                    !matches!(self.stack.last(), Some(Container::Mapping(_, None))),
                    "non-scalar YAML key"
                );
                self.stack
                    .push(if matches!(event, Event::MappingStart(..)) {
                        Container::Mapping(Map::new(), None)
                    } else {
                        Container::Sequence(Vec::new())
                    });
            }
            Event::MappingEnd | Event::SequenceEnd => {
                let value = match self.stack.pop().context("invalid YAML nesting")? {
                    Container::Mapping(map, key) => {
                        ensure!(key.is_none(), "YAML key without value");
                        Value::Object(map)
                    }
                    Container::Sequence(items) => Value::Array(items),
                };
                self.insert(value)?;
            }
            Event::Scalar(text, style, _, tag) => {
                ensure!(self.stack.len() <= 128, "Unity YAML nesting exceeds 128");
                if let Some(Container::Mapping(_, pending @ None)) = self.stack.last_mut() {
                    *pending = Some(text);
                    return Ok(());
                }
                let key = match self.stack.last() {
                    Some(Container::Mapping(_, Some(key))) => key.as_str(),
                    _ => "",
                };
                let value = if self.lexical {
                    Value::String(text)
                } else {
                    scalar(&text, key, style, tag)?
                };
                self.insert(value)?;
            }
            _ => {}
        }
        Ok(())
    }
}
pub fn parse_yaml_value(text: &str) -> Result<Value> {
    parse_yaml(text, false)
}

fn parse_yaml(text: &str, lexical: bool) -> Result<Value> {
    // These code points are refused while reading the stream.
    ensure!(
        !text.chars().any(|c| !matches!(c, '\t' | '\n' | '\r' | ' '..='~' | '\u{85}' | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..)),
        "yaml: control characters are not allowed"
    );
    // YAML 1.1 treats NEL, LS and PS as line breaks; the Rust parser does not.
    // Refuse them rather than read a different value than a YAML 1.1 reader would.
    ensure!(
        !text.contains(['\u{85}', '\u{2028}', '\u{2029}']),
        "yaml: unsupported Unicode line break"
    );
    let mut receiver = Receiver {
        lexical,
        ..Default::default()
    };
    let mut parser = Parser::new_from_str(text);
    // The parser's load() recursively walks containers before the receiver can
    // return errors. Consume events directly and stop at the depth bound.
    loop {
        let (event, _) = match parser.next_token() {
            Ok(token) => token,
            // YAML 1.1 ends a complete document at a directive line;
            // only the first document is read, so what follows is not parsed.
            Err(error)
                if receiver.stack.is_empty()
                    && receiver.root.is_some()
                    && error
                        .info()
                        .contains("missing explicit document end marker") =>
            {
                break;
            }
            Err(error) => bail!("yaml: {error}"),
        };
        // Only the first document is read.
        if matches!(event, Event::StreamEnd | Event::DocumentEnd) {
            break;
        }
        receiver.event(event)?;
    }
    ensure!(receiver.stack.is_empty(), "incomplete YAML object");
    receiver.root.ok_or_else(|| EmptyDocument.into())
}

/// The body held no YAML node at all.
#[derive(Debug)]
pub struct EmptyDocument;
impl std::fmt::Display for EmptyDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("requires one YAML object")
    }
}
impl std::error::Error for EmptyDocument {}

/// Simple per-character lower-casing.
fn lower(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    text.chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}
#[derive(PartialEq)]
enum Resolved {
    Null,
    Int,
    Float,
    Text,
    Merge,
}
static YAML_FLOAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$").unwrap());
/// The YAML 1.1 type of an untagged plain scalar.
fn resolve(text: &str) -> Resolved {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Resolved::Null,
        "true" | "True" | "TRUE" | "false" | "False" | "FALSE" => return Resolved::Text,
        ".nan" | ".NaN" | ".NAN" | ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF"
        | "-.inf" | "-.Inf" | "-.INF" => return Resolved::Float,
        "<<" => return Resolved::Merge,
        _ => {}
    }
    match text.as_bytes()[0] {
        b'.' => {
            if parse_float(text).is_ok() {
                return Resolved::Float;
            }
        }
        b'+' | b'-' | b'0'..=b'9' => {
            let plain = text.replace('_', "");
            if parse_int(&plain, 0).is_ok() || parse_uint(&plain, 0).is_ok() {
                return Resolved::Int;
            }
            if YAML_FLOAT.is_match(&plain) && parse_float(&plain).is_ok() {
                return Resolved::Float;
            }
            for (prefix, radix) in [("0b", 2), ("0o", 8)] {
                if let Some(rest) = plain.strip_prefix(prefix) {
                    if parse_int(rest, radix).is_ok() || parse_uint(rest, radix).is_ok() {
                        return Resolved::Int;
                    }
                } else if let Some(rest) =
                    plain.strip_prefix('-').and_then(|p| p.strip_prefix(prefix))
                    && parse_int(&format!("-{rest}"), radix).is_ok()
                {
                    return Resolved::Int;
                }
            }
        }
        _ => {}
    }
    Resolved::Text
}

fn scalar(text: &str, key: &str, style: TScalarStyle, tag: Option<Tag>) -> Result<Value> {
    if key == "_typelessdata" || key == "m_IndexBuffer" {
        return Ok(Value::String(text.into()));
    }
    if key == "fileID" {
        ensure!(parse_int(text, 10).is_ok(), "invalid fileID {text:?}");
        return Ok(Value::String(text.into()));
    }
    if key == "guid" {
        return Ok(Value::String(lower(text)));
    }
    // A lone `!` is the non-specific tag: plain scalars still resolve.
    let explicit = tag
        .as_ref()
        .map(|tag| format!("{}{}", tag.handle, tag.suffix))
        .filter(|tag| tag != "!");
    let resolved = match explicit.as_deref() {
        Some(tag) => match tag.strip_prefix("tag:yaml.org,2002:") {
            Some("str" | "bool" | "timestamp") => Resolved::Text,
            Some("null") => Resolved::Null,
            Some("int") => Resolved::Int,
            Some("float") => Resolved::Float,
            Some(short) => bail!("unsupported YAML tag !!{short}"),
            None => bail!("unsupported YAML tag {tag}"),
        },
        None if style != TScalarStyle::Plain => Resolved::Text,
        None => resolve(text),
    };
    match resolved {
        Resolved::Null => Ok(Value::Null),
        Resolved::Text => Ok(Value::String(text.into())),
        Resolved::Merge => bail!("unsupported YAML tag !!merge"),
        Resolved::Int => {
            if let Ok(value) = parse_int(&text.replace('_', ""), 0).or_else(|_| parse_int(text, 10))
            {
                Ok(Value::from(value))
            } else if let Ok(value) = parse_uint(text, 10) {
                Ok(Value::from(value))
            } else {
                bail!("integer {text:?} exceeds 64 bits")
            }
        }
        Resolved::Float => Ok(Value::Number(
            parse_float(text)
                .ok()
                .and_then(Number::from_f64)
                .with_context(|| format!("non-finite or invalid float {text:?}"))?,
        )),
    }
}
