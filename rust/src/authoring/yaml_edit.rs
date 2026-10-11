use super::{
    hierarchy::{field, id},
    store::*,
    yamlcodec::{self, Kind, Node, Style, internal},
};
use crate::yaml::{Document, parse_unity_yaml_with_comments};
use regex::Regex;
use serde_json::{Map, Number, Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};
static HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^--- !u!(\d+) &(-?\d+)(?: (stripped))?[ \t]*\r?$").unwrap());
pub(crate) static SEGMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^\x00\r\n.\[\]]{1,256})(?:\[(\d+)\])?$").unwrap());
#[derive(Clone, Debug)]
pub(crate) struct TextDocument {
    pub document: Document,
    pub header: String,
    pub body: String,
    /// The object's field mapping, positioned in `body`.
    pub node: Node,
}
impl TextDocument {
    /// A document whose text is authoritative until the asset is refreshed.
    pub fn raw(document: Document, header: String, body: String) -> Self {
        Self {
            document,
            header,
            body,
            node: Node::scalar("", yamlcodec::NULL, Style::Plain),
        }
    }
    pub fn id(&self) -> &str {
        &self.document.file_id
    }
    pub fn class_id(&self) -> i64 {
        self.document.class_id
    }
    pub fn stripped(&self) -> bool {
        self.document.stripped
    }
    pub fn data(&self) -> &Map<String, Value> {
        &self.document.data
    }
    pub fn field(&self, key: &str) -> &Value {
        field(&self.document.data, key)
    }
}
#[derive(Clone, Debug)]
pub(crate) struct TextAsset {
    pub prefix: String,
    pub docs: Vec<TextDocument>,
    pub newline: &'static str,
}
pub(crate) fn invalid(error: impl std::fmt::Display) -> AuthoringError {
    failure("invalid_yaml", error.to_string(), None)
}
pub(crate) fn reference(file_id: &str) -> Value {
    json!({"fileID": file_id})
}
pub(crate) fn reference_list(ids: &[String]) -> Value {
    Value::Array(ids.iter().map(|id| reference(id)).collect())
}
pub(crate) fn ids_from_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().map(id).collect())
        .unwrap_or_default()
}
pub(crate) fn items(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}
pub(crate) fn external_reference(value: &Value) -> bool {
    value["guid"]
        .as_str()
        .is_some_and(|guid| !guid.is_empty() && guid != "00000000000000000000000000000000")
}
pub(crate) fn protected(key: &str) -> bool {
    [
        "fileID",
        "guid",
        "serializedVersion",
        "m_GameObject",
        "m_Component",
        "m_Children",
        "m_Father",
        "m_Roots",
        "m_Script",
        "m_PrefabInstance",
        "m_PrefabAsset",
        "m_CorrespondingSourceObject",
        "m_SourcePrefab",
        "m_Modification",
        "m_AddedGameObjects",
        "m_RemovedGameObjects",
        "m_RemovedComponents",
        "m_AddedComponents",
    ]
    .contains(&key)
}
/// A mapping holding fileID is a reference and is not descended into.
pub(crate) fn visit_references(
    value: &Value,
    visit: &mut impl FnMut(&Value) -> Result<()>,
) -> Result<()> {
    match value {
        Value::Object(map) if map.contains_key("fileID") => visit(value),
        Value::Object(map) => map
            .values()
            .try_for_each(|child| visit_references(child, visit)),
        Value::Array(items) => items
            .iter()
            .try_for_each(|child| visit_references(child, visit)),
        _ => Ok(()),
    }
}
pub(crate) fn visit_map_references(
    map: &Map<String, Value>,
    visit: &mut impl FnMut(&Value) -> Result<()>,
) -> Result<()> {
    if map.contains_key("fileID") {
        // Document data is passed as a map; it is itself a reference only in name.
        return visit(&Value::Object(map.clone()));
    }
    map.values()
        .try_for_each(|child| visit_references(child, visit))
}
pub(crate) fn visit_references_mut(
    value: &mut Value,
    visit: &mut impl FnMut(&mut Map<String, Value>) -> Result<()>,
) -> Result<()> {
    match value {
        Value::Object(map) if map.contains_key("fileID") => visit(map),
        Value::Object(map) => map
            .values_mut()
            .try_for_each(|child| visit_references_mut(child, visit)),
        Value::Array(items) => items
            .iter_mut()
            .try_for_each(|child| visit_references_mut(child, visit)),
        _ => Ok(()),
    }
}
fn normalize_number(number: &Number, request: bool) -> Result<Value> {
    let text = number.to_string();
    if request {
        // Request numbers keep their text: an integer first, then a finite float.
        if let Ok(value) = text.parse::<i64>() {
            return Ok(Value::from(value));
        }
        return match text.parse::<f64>() {
            Ok(value) if value.is_finite() => Ok(Value::Number(yamlcodec::float_number(value)?)),
            _ => Err(internal("invalid JSON number")),
        };
    }
    if !yamlcodec::is_float(number) {
        return Ok(Value::Number(number.clone()));
    }
    let value = text
        .parse::<f64>()
        .map_err(|_| internal("invalid JSON number"))?;
    if value.trunc() == value && value.abs() <= (1u64 << 53) as f64 {
        Ok(Value::from(value as i64))
    } else {
        Ok(Value::Number(number.clone()))
    }
}
fn normalize_with(value: &Value, request: bool) -> Result<Value> {
    Ok(match value {
        Value::Bool(value) => Value::from(i64::from(*value)),
        Value::Number(number) => normalize_number(number, request)?,
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| normalize_with(item, request))
                .collect::<Result<_>>()?,
        ),
        Value::Object(map) => {
            if let Some(file_id) = map.get("fileID") {
                let text = file_id.as_str().ok_or_else(|| {
                    failure(
                        "invalid_identity",
                        "fileID values must be decimal strings",
                        None,
                    )
                })?;
                if text.parse::<i64>().is_err() {
                    return Err(failure(
                        "invalid_identity",
                        "fileID must fit a signed 64-bit integer",
                        None,
                    ));
                }
                if let Some(guid) = map.get("guid")
                    && !guid.as_str().is_some_and(|guid| {
                        guid.len() == 32 && guid.bytes().all(|b| b.is_ascii_hexdigit())
                    })
                {
                    return Err(failure(
                        "invalid_identity",
                        "reference GUID must be a 32-hex string",
                        None,
                    ));
                }
            }
            let reference = map.get("fileID").is_some_and(|value| !value.is_null());
            let mut out = Map::new();
            for (key, item) in map {
                let mut normalized = normalize_with(item, request)?;
                if key == "guid" && reference {
                    normalized = Value::String(item.as_str().unwrap_or_default().to_lowercase());
                }
                out.insert(key.clone(), normalized);
            }
            Value::Object(out)
        }
        _ => value.clone(),
    })
}
/// Normalizes a value decoded from a request, whose numbers are JSON number text.
pub(crate) fn normalize(value: &Value) -> Result<Value> {
    normalize_with(value, true)
}
/// Normalizes a value that already holds typed i64/u64/f64 numbers.
pub(crate) fn normalize_typed(value: &Value) -> Result<Value> {
    normalize_with(value, false)
}
pub(crate) fn json_numbers(value: &Value) -> Value {
    match value {
        Value::Number(number) if yamlcodec::is_float(number) => number
            .to_string()
            .parse::<f64>()
            .ok()
            .and_then(|value| serde_json::from_str(&yamlcodec::format_json(value)).ok())
            .map_or_else(|| value.clone(), Value::Number),
        Value::Array(items) => Value::Array(items.iter().map(json_numbers).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), json_numbers(value)))
                .collect(),
        ),
        _ => value.clone(),
    }
}
/// Copies document data through a JSON encode and decode with number text, then normalizes it.
pub(crate) fn json_round_trip(value: &Value) -> Result<Value> {
    normalize(&json_numbers(value))
}
fn line_start(body: &str, offset: usize) -> usize {
    body[..offset].rfind('\n').map_or(0, |index| index + 1)
}
fn column(body: &str, offset: usize) -> usize {
    body[line_start(body, offset)..offset].chars().count()
}
/// Keep comments and blank lines belonging to the next field byte-for-byte.
pub(crate) fn trim_trailing_comments(body: &str, start: usize, mut end: usize) -> usize {
    let bytes = body.as_bytes();
    while end > start {
        let mut last = end;
        if bytes[last - 1] == b'\n' {
            last -= 1;
        }
        if last > start && bytes[last - 1] == b'\r' {
            last -= 1;
        }
        let begin = line_start(body, last);
        let line = body[begin..last].trim();
        if line.is_empty() || line.starts_with('#') {
            end = begin;
        } else {
            break;
        }
    }
    end
}
/// The exact lexical extent of an existing scalar.
pub(crate) fn scalar_end(body: &str, node: &Node) -> Result<usize> {
    if matches!(node.style, Style::Literal | Style::Folded) {
        return Err(failure(
            "unsupported_property",
            "block scalar edits require script/file tooling",
            None,
        ));
    }
    let bytes = body.as_bytes();
    let start = node.start;
    if start >= bytes.len() {
        return Err(internal("invalid YAML scalar position"));
    }
    if node.empty {
        return Err(failure(
            "unsupported_property",
            "empty implicit scalar is unsupported",
            None,
        ));
    }
    let quote = bytes[start];
    if quote == b'\'' || quote == b'"' {
        let mut index = start + 1;
        while index < bytes.len() {
            if quote == b'"' && bytes[index] == b'\\' {
                index += 2;
                continue;
            }
            if bytes[index] != quote {
                index += 1;
                continue;
            }
            if quote == b'\'' && bytes.get(index + 1) == Some(&b'\'') {
                index += 2;
                continue;
            }
            return Ok(index + 1);
        }
        return Err(internal("unterminated quoted YAML scalar"));
    }
    if node.tag == yamlcodec::STR {
        let end = start + node.value.len();
        if body.get(start..end) == Some(node.value.as_str()) {
            return Ok(end);
        }
        return Err(failure(
            "unsupported_property",
            "multiline plain strings are unsupported",
            None,
        ));
    }
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'\n' && bytes[end] != b'\r' {
        let c = bytes[end];
        if c == b','
            || c == b'}'
            || c == b']'
            || (c == b'#' && (end == start || matches!(bytes[end - 1], b' ' | b'\t')))
        {
            break;
        }
        end += 1;
    }
    while end > start && matches!(bytes[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    if end == start {
        return Err(failure(
            "unsupported_property",
            "empty implicit scalar is unsupported",
            None,
        ));
    }
    Ok(end)
}
/// The lexical end of a flow collection starting at `start`.
pub(crate) fn flow_end(body: &str, start: usize, comments: bool) -> Option<usize> {
    let bytes = body.as_bytes();
    let (mut quote, mut depth) = (0u8, 0i32);
    let mut index = start;
    while index < bytes.len() {
        let c = bytes[index];
        if quote != 0 {
            if quote == b'"' && c == b'\\' {
                index += 2;
                continue;
            }
            if c == quote {
                if quote == b'\'' && bytes.get(index + 1) == Some(&b'\'') {
                    index += 2;
                    continue;
                }
                quote = 0;
            }
        } else if c == b'\'' || c == b'"' {
            quote = c;
        } else if comments
            && c == b'#'
            && (index == start || matches!(bytes[index - 1], b' ' | b'\t'))
        {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
        } else if c == b'{' || c == b'[' {
            depth += 1;
        } else if c == b'}' || c == b']' {
            depth -= 1;
            if depth == 0 {
                return Some(index + 1);
            }
        }
        index += 1;
    }
    None
}
pub(crate) fn parse_property(path: &str) -> Result<Vec<&str>> {
    if path.len() > 1024 || path.is_empty() {
        return Err(failure(
            "unsupported_property",
            "property path is empty or too long",
            None,
        ));
    }
    let parts: Vec<&str> = path.split('.').collect();
    if parts.iter().any(|part| !SEGMENT.is_match(part)) {
        return Err(failure(
            "unsupported_property",
            "use field.subfield or field[index] property paths",
            None,
        ));
    }
    Ok(parts)
}
fn resolve_property<'a>(root: &'a Node, parts: &[&str]) -> Result<&'a Node> {
    let mut current = root;
    for part in parts {
        let capture = SEGMENT.captures(part).unwrap();
        current = current
            .field(&capture[1])
            .ok_or_else(|| {
                failure(
                    "unsupported_property",
                    "property path is absent in authored data",
                    None,
                )
            })?
            .0;
        if let Some(index) = capture.get(2) {
            let absent = || {
                failure(
                    "unsupported_property",
                    "array element is absent in authored data",
                    None,
                )
            };
            let index = index.as_str().parse::<usize>().map_err(|_| absent())?;
            if current.kind != Kind::Sequence {
                return Err(absent());
            }
            current = current.content.get(index).ok_or_else(absent)?;
        }
    }
    Ok(current)
}
impl TextAsset {
    pub fn empty(prefix: &str, newline: &'static str) -> Self {
        Self {
            prefix: prefix.into(),
            docs: Vec::new(),
            newline,
        }
    }
    pub fn parse(content: &str) -> Result<Self> {
        let parsed = parse_unity_yaml_with_comments(content.as_bytes())
            .map_err(|e| invalid(format!("{e:#}")))?;
        let headers: Vec<_> = HEADER.find_iter(content).collect();
        if headers.len() != parsed.len() {
            return Err(invalid("document spans are inconsistent"));
        }
        let mut docs = Vec::with_capacity(parsed.len());
        for (index, document) in parsed.into_iter().enumerate() {
            let start = headers[index].start();
            let end = headers
                .get(index + 1)
                .map_or(content.len(), |next| next.start());
            let mut body_start = headers[index].end();
            if body_start < end && content.as_bytes()[body_start] == b'\n' {
                body_start += 1;
            }
            let body = content[body_start..end].to_string();
            let mut tree = yamlcodec::parse_tree(&body)?;
            if tree.kind != Kind::Mapping || tree.content.len() != 2 {
                return Err(invalid(format!(
                    "fileID {} requires one object mapping",
                    document.file_id
                )));
            }
            docs.push(TextDocument {
                document,
                header: content[start..body_start].into(),
                body,
                node: tree.content.pop().unwrap(),
            });
        }
        Ok(Self {
            prefix: content[..headers[0].start()].into(),
            docs,
            newline: if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            },
        })
    }
    pub fn text(&self) -> String {
        let mut output = self.prefix.clone();
        for doc in &self.docs {
            output.push_str(&doc.header);
            output.push_str(&doc.body);
        }
        output
    }
    pub fn refresh(&mut self) -> Result<()> {
        *self = Self::parse(&self.text())?;
        Ok(())
    }
    pub fn position(&self, file_id: &str) -> Option<usize> {
        self.docs.iter().position(|doc| doc.id() == file_id)
    }
    pub fn find(&self, file_id: &str) -> Option<&TextDocument> {
        self.docs.iter().find(|doc| doc.id() == file_id)
    }
    pub fn contains(&self, file_id: &str) -> bool {
        self.find(file_id).is_some()
    }
    pub fn stripped_header(&self, class_id: i64, file_id: &str) -> String {
        format!("--- !u!{class_id} &{file_id} stripped{}", self.newline)
    }
    pub fn append_document(
        &mut self,
        class_id: i64,
        file_id: &str,
        type_name: &str,
        data: &Map<String, Value>,
    ) -> Result<()> {
        let mut outer = Map::new();
        outer.insert(type_name.into(), Value::Object(data.clone()));
        let node = yamlcodec::document_node(&Value::Object(outer))?;
        // The emitter sorts supplied maps; this affects only the newly created object.
        let mut text = yamlcodec::encode_node(&node)?.replace('\n', self.newline);
        text.push_str(self.newline);
        if let Some(last) = self.docs.last_mut()
            && !last.body.is_empty()
            && !last.body.ends_with('\n')
        {
            last.body.push_str(self.newline);
        }
        let document = Document {
            class_id,
            file_id: file_id.into(),
            type_name: type_name.into(),
            data: Map::new(),
            stripped: false,
        };
        let header = format!("--- !u!{class_id} &{file_id}{}", self.newline);
        self.docs.push(TextDocument::raw(document, header, text));
        self.refresh()
    }
    pub fn set_top_level(&mut self, file_id: &str, key: &str, value: &Value) -> Result<()> {
        let position = self.position(file_id).ok_or_else(|| {
            failure(
                "not_found",
                "serialized object does not exist",
                Some(json!({"fileId": file_id})),
            )
        })?;
        let doc = &self.docs[position];
        let (_, index) = doc.node.field(key).ok_or_else(|| {
            failure(
                "unsupported_property",
                "property does not exist in authored object",
                Some(json!({"fileId": file_id, "property": key})),
            )
        })?;
        let key_node = &doc.node.content[index];
        let start = line_start(&doc.body, key_node.start);
        let end = doc
            .node
            .content
            .get(index + 2)
            .map_or(doc.body.len(), |next| line_start(&doc.body, next.start));
        let end = trim_trailing_comments(&doc.body, start, end);
        let text = yamlcodec::encode_node(&yamlcodec::value_node(value)?)?;
        let pad = " ".repeat(column(&doc.body, key_node.start));
        let text = text.replace('\n', &format!("{}{pad}", self.newline));
        let replacement = format!("{pad}{key}: {text}{}", self.newline);
        self.docs[position]
            .body
            .replace_range(start..end, &replacement);
        self.refresh()
    }
    pub fn set_property(&mut self, file_id: &str, path: &str, value: &Value) -> Result<Value> {
        self.set_property_mode(file_id, path, value, true)
    }
    /// `value` holds typed numbers rather than request number text.
    pub fn set_property_typed(
        &mut self,
        file_id: &str,
        path: &str,
        value: &Value,
    ) -> Result<Value> {
        self.set_property_mode(file_id, path, value, false)
    }
    fn set_property_mode(
        &mut self,
        file_id: &str,
        path: &str,
        value: &Value,
        request: bool,
    ) -> Result<Value> {
        let position = self.position(file_id).ok_or_else(|| {
            failure(
                "not_found",
                "serialized object does not exist",
                Some(json!({"fileId": file_id})),
            )
        })?;
        let doc = &self.docs[position];
        if doc.stripped() || id(doc.field("m_PrefabInstance")) != "0" {
            return Err(failure(
                "unsupported_prefab_write",
                "prefab instance and stripped objects require override-aware authoring",
                Some(json!({"fileId": file_id})),
            ));
        }
        let parts = parse_property(path)?;
        if parts
            .iter()
            .any(|part| protected(&SEGMENT.captures(part).unwrap()[1]))
        {
            return Err(failure(
                "protected_property",
                "use graph operations for object identity and ownership",
                Some(json!({"property": path})),
            ));
        }
        let old = resolve_property(&doc.node, &parts)?;
        let before = json_numbers(&yamlcodec::decode(old)?);
        let value = normalize_with(value, request)?;
        let string = value.is_string();
        let mut new_node = yamlcodec::value_node(&value)?;
        if old.kind != Kind::Scalar {
            if new_node.kind != old.kind {
                return Err(failure(
                    "invalid_property_value",
                    "collection replacement must preserve mapping or array shape",
                    None,
                ));
            }
            let old = old.clone();
            self.replace_collection(position, &old, &new_node)?;
            return Ok(before);
        }
        if new_node.kind != Kind::Scalar {
            return Err(failure(
                "invalid_property_value",
                "a scalar property requires a scalar value",
                None,
            ));
        }
        if old.tag == yamlcodec::STR {
            if !string {
                return Err(failure(
                    "invalid_property_value",
                    "a string property requires a string",
                    None,
                ));
            }
            new_node.tag = yamlcodec::STR.into();
            new_node.style = Style::Double;
        }
        let numeric = new_node.tag == yamlcodec::INT || new_node.tag == yamlcodec::FLOAT;
        if old.tag == yamlcodec::INT {
            // Unity serializes integral-valued floats as e.g. x: 0. Lexical YAML
            // integer tags cannot establish a Unity field's numeric type.
            if !numeric {
                return Err(failure(
                    "invalid_property_value",
                    "a numeric property requires a number",
                    None,
                ));
            }
            if ["m_Layer", "m_IsActive", "m_Enabled", "m_ObjectHideFlags"].contains(&path)
                && new_node.tag != yamlcodec::INT
            {
                return Err(failure(
                    "invalid_property_value",
                    "this Unity property requires an integer",
                    None,
                ));
            }
        }
        if old.tag == yamlcodec::FLOAT && !numeric {
            return Err(failure(
                "invalid_property_value",
                "a numeric property requires a number",
                None,
            ));
        }
        let start = old.start;
        let end = scalar_end(&doc.body, old)?;
        let text = yamlcodec::encode_node(&new_node)?;
        if text.contains('\n') {
            return Err(failure(
                "unsupported_property",
                "multiline scalar edits are unsupported",
                None,
            ));
        }
        self.docs[position].body.replace_range(start..end, &text);
        self.refresh()?;
        Ok(before)
    }
    /// Change the selected collection's lexical span, keeping surrounding
    /// fields and comments, including siblings in the same flow mapping.
    fn replace_collection(
        &mut self,
        position: usize,
        old: &Node,
        replacement: &Node,
    ) -> Result<()> {
        let doc = &self.docs[position];
        let start = old.start;
        let block = !old.flow;
        let end = if block {
            let mut end = doc.body.len();
            doc.node.visit(&mut |node| {
                if (node.index < old.index || node.index > old.last) && node.start > start {
                    end = end.min(line_start(&doc.body, node.start));
                }
            });
            trim_trailing_comments(&doc.body, start, end)
        } else {
            flow_end(&doc.body, start, true).ok_or_else(|| {
                failure(
                    "invalid_yaml",
                    "collection lexical span could not be established",
                    None,
                )
            })?
        };
        let mut text = yamlcodec::encode_node(replacement)?;
        if block {
            text.push_str(self.newline);
        }
        self.docs[position].body.replace_range(start..end, &text);
        self.refresh()
    }
    /// Unity intentionally omits GameObject/parent/children fields from stripped
    /// prefab documents. Preserve those records instead of inventing ownership.
    pub fn preserve_prefab_documents(&self, original: &Self) -> Result<()> {
        self.preserve_prefab_documents_except(original, &HashSet::new())
    }
    pub fn preserve_prefab_documents_except(
        &self,
        original: &Self,
        allowed: &HashSet<String>,
    ) -> Result<()> {
        for before in &original.docs {
            if !before.stripped() && before.class_id() != 1001 {
                continue;
            }
            if before.class_id() == 1001 && allowed.contains(before.id()) {
                continue;
            }
            if self
                .find(before.id())
                .is_none_or(|after| after.header != before.header || after.body != before.body)
            {
                return Err(failure(
                    "unsupported_prefab_write",
                    "prefab instance and stripped records must remain byte-for-byte unchanged",
                    Some(json!({"fileId": before.id()})),
                ));
            }
        }
        Ok(())
    }
    pub fn root_document(&self) -> Option<&TextDocument> {
        self.docs.iter().find(|doc| doc.class_id() == 1660057539)
    }
    pub fn validate_prefab_root(&self) -> Result<()> {
        let mut roots = 0;
        for doc in &self.docs {
            if doc.class_id() == 1660057539 {
                return Err(failure(
                    "invalid_prefab_root",
                    "prefab cannot contain a scene root registry",
                    None,
                ));
            }
            if [4, 224].contains(&doc.class_id())
                && !doc.stripped()
                && id(doc.field("m_Father")) == "0"
            {
                roots += 1;
            }
            if doc.class_id() == 1001
                && id(&doc.field("m_Modification")["m_TransformParent"]) == "0"
            {
                roots += 1;
            }
        }
        if roots != 1 {
            return Err(failure(
                "invalid_prefab_root",
                "prefab must retain exactly one authored or inherited root",
                None,
            ));
        }
        Ok(())
    }
    pub fn validate(&self, asset_guid: &str) -> Result<()> {
        let by_id: HashMap<&str, &TextDocument> =
            self.docs.iter().map(|doc| (doc.id(), doc)).collect();
        let mut transforms: Vec<&TextDocument> = Vec::new();
        for doc in &self.docs {
            visit_map_references(doc.data(), &mut |reference| {
                let target = id(reference);
                let guid = reference["guid"].as_str().unwrap_or_default();
                if target != "0"
                    && (!external_reference(reference) || guid == asset_guid)
                    && !by_id.contains_key(target.as_str())
                {
                    return Err(failure(
                        "dangling_reference",
                        "edit would leave a reference to an absent local object",
                        Some(json!({"sourceFileId": doc.id(), "targetFileId": target})),
                    ));
                }
                Ok(())
            })?;
            if [4, 224].contains(&doc.class_id()) {
                transforms.push(doc);
            }
            if doc.class_id() == 1 && !doc.stripped() {
                let mut seen = HashSet::new();
                let mut transform_count = 0;
                for entry in items(doc.field("m_Component")) {
                    let component_id = id(&entry["component"]);
                    let component = by_id.get(component_id.as_str());
                    if component.is_none_or(|component| {
                        seen.contains(&component_id)
                            || (!component.stripped()
                                && id(component.field("m_GameObject")) != doc.id())
                    }) {
                        return Err(failure(
                            "invalid_graph",
                            "component ownership or identity is invalid",
                            Some(json!({"gameObjectId": doc.id(), "componentId": component_id})),
                        ));
                    }
                    if [4, 224].contains(&component.unwrap().class_id()) {
                        transform_count += 1;
                    }
                    seen.insert(component_id);
                }
                if transform_count != 1 {
                    return Err(failure(
                        "invalid_graph",
                        "GameObject requires exactly one Transform",
                        Some(json!({"gameObjectId": doc.id()})),
                    ));
                }
            }
        }
        let transform = |file_id: &str| {
            by_id
                .get(file_id)
                .filter(|doc| [4, 224].contains(&doc.class_id()))
        };
        for doc in &transforms {
            if doc.stripped() {
                continue;
            }
            let parent = id(doc.field("m_Father"));
            if parent != "0" {
                let parent = transform(&parent).ok_or_else(|| {
                    failure("invalid_graph", "Transform parent is not a Transform", None)
                })?;
                let found = ids_from_list(parent.field("m_Children"))
                    .iter()
                    .filter(|child| child.as_str() == doc.id())
                    .count();
                if !parent.stripped() && found != 1 {
                    return Err(failure(
                        "invalid_graph",
                        "Transform parent and children disagree",
                        Some(json!({"fileId": doc.id()})),
                    ));
                }
            }
            let mut seen = HashSet::new();
            let mut current = doc.id().to_string();
            while current != "0" {
                if !seen.insert(current.clone()) {
                    return Err(failure(
                        "transform_cycle",
                        "Transform ancestry contains a cycle",
                        None,
                    ));
                }
                let Some(ancestor) = transform(&current) else {
                    break;
                };
                current = id(ancestor.field("m_Father"));
            }
            let mut seen = HashSet::new();
            for child in ids_from_list(doc.field("m_Children")) {
                if seen.contains(&child)
                    || transform(&child).is_none_or(|candidate| {
                        !candidate.stripped() && id(candidate.field("m_Father")) != doc.id()
                    })
                {
                    return Err(failure(
                        "invalid_graph",
                        "Transform children and parent disagree",
                        Some(json!({"fileId": doc.id(), "childId": child})),
                    ));
                }
                seen.insert(child);
            }
        }
        if let Some(roots) = self.root_document() {
            let mut seen = HashSet::new();
            for root in ids_from_list(roots.field("m_Roots")) {
                let found = transform(&root);
                let mut instance = by_id.get(root.as_str());
                if let Some(found) = found.filter(|found| found.stripped()) {
                    instance = by_id.get(id(found.field("m_PrefabInstance")).as_str());
                }
                let identity = if let Some(instance) =
                    instance.filter(|instance| instance.class_id() == 1001)
                {
                    if id(&instance.field("m_Modification")["m_TransformParent"]) != "0" {
                        return Err(failure(
                            "invalid_graph",
                            "SceneRoots contains a parented prefab instance",
                            None,
                        ));
                    }
                    format!("prefab:{}", instance.id())
                } else if found
                    .is_none_or(|found| !found.stripped() && id(found.field("m_Father")) != "0")
                {
                    return Err(failure(
                        "invalid_graph",
                        "SceneRoots contains an invalid root",
                        None,
                    ));
                } else {
                    root
                };
                if !seen.insert(identity) {
                    return Err(failure(
                        "invalid_graph",
                        "SceneRoots contains a duplicate root identity",
                        None,
                    ));
                }
            }
            for doc in &transforms {
                if !doc.stripped() && id(doc.field("m_Father")) == "0" && !seen.contains(doc.id()) {
                    return Err(failure(
                        "invalid_graph",
                        "root Transform is absent from SceneRoots",
                        Some(json!({"fileId": doc.id()})),
                    ));
                }
            }
            for instance in &self.docs {
                if instance.class_id() == 1001
                    && id(&instance.field("m_Modification")["m_TransformParent"]) == "0"
                    && !seen.contains(&format!("prefab:{}", instance.id()))
                {
                    return Err(failure(
                        "invalid_graph",
                        "root prefab instance is absent from SceneRoots",
                        Some(json!({"fileId": instance.id()})),
                    ));
                }
            }
        }
        Ok(())
    }
}
