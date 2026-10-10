//! The JSON Schema support the server uses: decoding a schema, resolving its
//! references and validating instances for draft-07 and draft 2020-12, with
//! fixed error texts.
//!
//! Instances are decoded JSON: every number is an f64. Where a map is iterated
//! (properties, patterns, dependencies) this module iterates in key order.
//! Patterns compile with the `regex` crate.
use crate::jsondoc::quoted_text;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

const DRAFT7: &str = "http://json-schema.org/draft-07/schema#";
const DRAFT7_SECURE: &str = "https://json-schema.org/draft-07/schema#";
const DRAFT2020: &str = "https://json-schema.org/draft/2020-12/schema";
// The type name shown in the error for a schema of the wrong JSON type.
const WIRE_STRUCT: &str = "schema";

#[derive(Debug, Default)]
struct Node {
    path: String,
    zero: bool,
    id: String,
    schema: String,
    reference: String,
    anchor: String,
    dynamic_anchor: String,
    dynamic_ref: String,
    vocabulary: bool,
    kind: String,
    kinds: Option<Vec<String>>,
    enumeration: Option<Vec<Value>>,
    constant: Option<Value>,
    multiple_of: Option<f64>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    exclusive_minimum: Option<f64>,
    exclusive_maximum: Option<f64>,
    min_length: Option<i64>,
    max_length: Option<i64>,
    pattern: String,
    prefix_items: Option<Vec<usize>>,
    items: Option<usize>,
    items_array: Option<Vec<usize>>,
    min_items: Option<i64>,
    max_items: Option<i64>,
    additional_items: Option<usize>,
    unique_items: bool,
    contains: Option<usize>,
    min_contains: Option<i64>,
    max_contains: Option<i64>,
    unevaluated_items: Option<usize>,
    min_properties: Option<i64>,
    max_properties: Option<i64>,
    required: Option<Vec<String>>,
    dependent_required: Option<BTreeMap<String, Vec<String>>>,
    properties: Option<BTreeMap<String, usize>>,
    pattern_properties: Option<BTreeMap<String, usize>>,
    additional_properties: Option<usize>,
    property_names: Option<usize>,
    unevaluated_properties: Option<usize>,
    all_of: Option<Vec<usize>>,
    any_of: Option<Vec<usize>>,
    one_of: Option<Vec<usize>>,
    not: Option<usize>,
    if_: Option<usize>,
    then: Option<usize>,
    else_: Option<usize>,
    dependent_schemas: Option<BTreeMap<String, usize>>,
    dependency_schemas: Option<BTreeMap<String, usize>>,
    dependency_strings: Option<BTreeMap<String, Vec<String>>>,
    defs: Option<BTreeMap<String, usize>>,
    definitions: Option<BTreeMap<String, usize>>,
    content_schema: Option<usize>,
    // Computed by resolve.
    base: usize,
    uri: Option<Uri>,
    anchors: BTreeMap<String, (usize, bool)>,
    resolved_ref: Option<usize>,
    resolved_dynamic_ref: Option<usize>,
    dynamic_ref_anchor: String,
    regex: Option<Regex>,
    pattern_regexes: Vec<(Regex, usize)>,
}

/// A schema with references resolved, ready for validation.
#[derive(Debug)]
pub struct Resolved {
    nodes: Vec<Node>,
    draft7: bool,
    uris: BTreeMap<String, usize>,
}

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
fn mismatch(value: &Value, field: &str, target_type: &str) -> String {
    format!(
        "json: cannot unmarshal {} into field schema.{field} of type {target_type}",
        kind_name(value)
    )
}

struct Builder {
    nodes: Vec<Node>,
}
impl Builder {
    fn text(value: &Value, field: &str) -> Result<String, String> {
        match value {
            Value::Null => Ok(String::new()),
            Value::String(text) => Ok(text.clone()),
            other => Err(mismatch(other, field, "string")),
        }
    }
    fn flag(value: &Value, field: &str) -> Result<bool, String> {
        match value {
            Value::Null => Ok(false),
            Value::Bool(flag) => Ok(*flag),
            other => Err(mismatch(other, field, "bool")),
        }
    }
    fn float(value: &Value, field: &str) -> Result<Option<f64>, String> {
        match value {
            Value::Null => Ok(None),
            Value::Number(number) => number
                .as_f64()
                .filter(|number| number.is_finite())
                .map(Some)
                .ok_or_else(|| mismatch(value, field, "float64")),
            other => Err(mismatch(other, field, "float64")),
        }
    }
    // An integer keyword: a whole number written with a fraction is accepted,
    // `null` is zero, and the value must fit in 32 bits.
    fn integer(value: &Value) -> Result<Option<i64>, String> {
        let text = match value {
            Value::Null => "null".to_string(),
            Value::Number(number) => number.to_string(),
            _ => return Err("cannot be unmarshaled into an int".into()),
        };
        let number = if text.contains('.') {
            let float: f64 = text.parse().map_err(|_| "not a number".to_string())?;
            let integer = float as i64;
            if integer as f64 != float {
                return Err("not an integer value".into());
            }
            integer
        } else if text == "null" {
            0
        } else {
            text.parse::<i64>()
                .map_err(|_| "cannot be unmarshaled into an int".to_string())?
        };
        if number < i32::MIN as i64 || number > i32::MAX as i64 {
            return Err("integer is out of range".into());
        }
        Ok(Some(number))
    }
    fn strings(value: &Value, field: &str) -> Result<Option<Vec<String>>, String> {
        match value {
            Value::Null => Ok(None),
            Value::Array(items) => items
                .iter()
                .map(|item| Self::text(item, field))
                .collect::<Result<_, _>>()
                .map(Some),
            other => Err(mismatch(other, field, "array")),
        }
    }
    fn string_lists(
        value: &Value,
        field: &str,
    ) -> Result<Option<BTreeMap<String, Vec<String>>>, String> {
        match value {
            Value::Null => Ok(None),
            Value::Object(members) => {
                let mut result = BTreeMap::new();
                for (key, item) in members {
                    result.insert(key.clone(), Self::strings(item, field)?.unwrap_or_default());
                }
                Ok(Some(result))
            }
            other => Err(mismatch(other, field, "object")),
        }
    }
    fn child(&mut self, value: &Value, field: &str) -> Result<Option<usize>, String> {
        match value {
            Value::Null => Ok(None),
            other => self.schema(other, field).map(Some),
        }
    }
    fn list(&mut self, value: &Value, field: &str) -> Result<Option<Vec<usize>>, String> {
        match value {
            Value::Null => Ok(None),
            Value::Array(items) => {
                let mut result = Vec::new();
                for item in items {
                    result.push(self.schema(item, field)?);
                }
                Ok(Some(result))
            }
            other => Err(mismatch(other, field, "array")),
        }
    }
    fn map(
        &mut self,
        value: &Value,
        field: &str,
    ) -> Result<Option<BTreeMap<String, usize>>, String> {
        match value {
            Value::Null => Ok(None),
            Value::Object(members) => {
                let mut result = BTreeMap::new();
                for (key, item) in members {
                    result.insert(key.clone(), self.schema(item, field)?);
                }
                Ok(Some(result))
            }
            other => Err(mismatch(other, field, "object")),
        }
    }
    fn push(&mut self, node: Node) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }
    // Decodes one schema. Children are created before their parent; the
    // root is moved to index 0 by `Resolved::parse`.
    fn schema(&mut self, value: &Value, field: &str) -> Result<usize, String> {
        let members = match value {
            Value::Bool(true) => {
                return Ok(self.push(Node {
                    zero: true,
                    ..Node::default()
                }));
            }
            Value::Bool(false) => {
                let not = self.push(Node {
                    zero: true,
                    ..Node::default()
                });
                return Ok(self.push(Node {
                    not: Some(not),
                    ..Node::default()
                }));
            }
            Value::Object(members) => members,
            other => {
                let _ = field;
                return Err(format!(
                    "json: cannot unmarshal {} into value of type {WIRE_STRUCT}",
                    kind_name(other)
                ));
            }
        };
        let mut node = Node {
            zero: members.is_empty(),
            ..Node::default()
        };
        for (key, item) in members {
            let key = key.as_str();
            match key {
                "$id" => node.id = Self::text(item, key)?,
                "$schema" => node.schema = Self::text(item, key)?,
                "$ref" => node.reference = Self::text(item, key)?,
                "$anchor" => node.anchor = Self::text(item, key)?,
                "$dynamicAnchor" => node.dynamic_anchor = Self::text(item, key)?,
                "$dynamicRef" => node.dynamic_ref = Self::text(item, key)?,
                "$comment" | "title" | "description" | "contentEncoding" | "contentMediaType"
                | "format" => {
                    Self::text(item, key)?;
                }
                "deprecated" | "readOnly" | "writeOnly" => {
                    Self::flag(item, key)?;
                }
                "$vocabulary" => match item {
                    Value::Null => {}
                    Value::Object(entries) => {
                        for entry in entries.values() {
                            Self::flag(entry, key)?;
                        }
                        node.vocabulary = true;
                    }
                    other => return Err(mismatch(other, key, "object")),
                },
                "examples" => match item {
                    Value::Null | Value::Array(_) => {}
                    other => return Err(mismatch(other, key, "array")),
                },
                "default" => {}
                "type" => match item {
                    Value::String(text) => node.kind = text.clone(),
                    Value::Array(_) => node.kinds = Self::strings(item, key)?,
                    Value::Null => {}
                    other => {
                        return Err(format!(
                            "invalid value for \"type\": {}",
                            quoted_text(&other.to_string())
                        ));
                    }
                },
                "enum" => match item {
                    Value::Null => {}
                    Value::Array(items) => node.enumeration = Some(items.clone()),
                    other => return Err(mismatch(other, key, "array")),
                },
                "const" => node.constant = Some(item.clone()),
                "multipleOf" => node.multiple_of = Self::float(item, key)?,
                "minimum" => node.minimum = Self::float(item, key)?,
                "maximum" => node.maximum = Self::float(item, key)?,
                "exclusiveMinimum" => node.exclusive_minimum = Self::float(item, key)?,
                "exclusiveMaximum" => node.exclusive_maximum = Self::float(item, key)?,
                "minLength" => node.min_length = Self::integer(item)?,
                "maxLength" => node.max_length = Self::integer(item)?,
                "minItems" => node.min_items = Self::integer(item)?,
                "maxItems" => node.max_items = Self::integer(item)?,
                "minContains" => node.min_contains = Self::integer(item)?,
                "maxContains" => node.max_contains = Self::integer(item)?,
                "minProperties" => node.min_properties = Self::integer(item)?,
                "maxProperties" => node.max_properties = Self::integer(item)?,
                "pattern" => node.pattern = Self::text(item, key)?,
                "uniqueItems" => node.unique_items = Self::flag(item, key)?,
                "required" => node.required = Self::strings(item, key)?,
                "dependentRequired" => node.dependent_required = Self::string_lists(item, key)?,
                "prefixItems" => node.prefix_items = self.list(item, key)?,
                "items" => match item {
                    Value::Array(_) => node.items_array = self.list(item, key)?,
                    Value::Null => {}
                    other => node.items = Some(self.schema(other, key)?),
                },
                "additionalItems" => node.additional_items = self.child(item, key)?,
                "contains" => node.contains = self.child(item, key)?,
                "unevaluatedItems" => node.unevaluated_items = self.child(item, key)?,
                "properties" => node.properties = self.map(item, key)?,
                "patternProperties" => node.pattern_properties = self.map(item, key)?,
                "additionalProperties" => node.additional_properties = self.child(item, key)?,
                "propertyNames" => node.property_names = self.child(item, key)?,
                "unevaluatedProperties" => node.unevaluated_properties = self.child(item, key)?,
                "allOf" => node.all_of = self.list(item, key)?,
                "anyOf" => node.any_of = self.list(item, key)?,
                "oneOf" => node.one_of = self.list(item, key)?,
                "not" => node.not = self.child(item, key)?,
                "if" => node.if_ = self.child(item, key)?,
                "then" => node.then = self.child(item, key)?,
                "else" => node.else_ = self.child(item, key)?,
                "dependentSchemas" => node.dependent_schemas = self.map(item, key)?,
                "$defs" => node.defs = self.map(item, key)?,
                "definitions" => node.definitions = self.map(item, key)?,
                "contentSchema" => node.content_schema = self.child(item, key)?,
                "dependencies" => match item {
                    Value::Null => {}
                    Value::Object(entries) => {
                        for (name, entry) in entries {
                            if entry.is_array() {
                                node.dependency_strings.get_or_insert_default().insert(
                                    name.clone(),
                                    Self::strings(entry, "")?.unwrap_or_default(),
                                );
                            } else {
                                let schema = self.schema(entry, "")?;
                                node.dependency_schemas
                                    .get_or_insert_default()
                                    .insert(name.clone(), schema);
                            }
                        }
                    }
                    other => {
                        return Err(mismatch(other, key, "object"));
                    }
                },
                _ => {}
            }
        }
        Ok(self.push(node))
    }
}

/// A URI reference: enough of RFC 3986 for `$id`/`$ref`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Uri {
    scheme: String,
    authority: Option<String>,
    path: String,
    query: Option<String>,
    fragment: String,
}
impl Uri {
    fn parse(text: &str) -> Result<Uri, String> {
        let invalid = |reason: &str| format!("parse {}: {reason}", quoted_text(text));
        if text.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
            return Err(invalid("net/url: invalid control character in URL"));
        }
        let (rest, fragment) = match text.split_once('#') {
            Some((rest, fragment)) => (rest, unescape(fragment).map_err(|e| invalid(&e))?),
            None => (text, String::new()),
        };
        let mut uri = Uri {
            fragment,
            ..Uri::default()
        };
        let mut rest = rest;
        if let Some(index) = rest.find([':', '/', '?'])
            && rest.as_bytes()[index] == b':'
            && index > 0
            && rest.as_bytes()[0].is_ascii_alphabetic()
            && rest[..index]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
        {
            uri.scheme = rest[..index].to_ascii_lowercase();
            rest = &rest[index + 1..];
        }
        if let Some((before, query)) = rest.split_once('?') {
            uri.query = Some(query.to_string());
            rest = before;
        }
        if let Some(after) = rest.strip_prefix("//") {
            let end = after.find('/').unwrap_or(after.len());
            uri.authority = Some(after[..end].to_string());
            rest = &after[end..];
        }
        uri.path = rest.to_string();
        Ok(uri)
    }
    fn absolute(&self) -> bool {
        !self.scheme.is_empty()
    }
    fn fragmentless(&self) -> String {
        let mut text = String::new();
        if !self.scheme.is_empty() {
            text.push_str(&self.scheme);
            text.push(':');
        }
        if let Some(authority) = &self.authority {
            text.push_str("//");
            text.push_str(authority);
        }
        text.push_str(&self.path);
        if let Some(query) = &self.query {
            text.push('?');
            text.push_str(query);
        }
        text
    }
    // RFC 3986 section 5.2.2.
    fn resolve(&self, reference: &Uri) -> Uri {
        let mut target = Uri {
            fragment: reference.fragment.clone(),
            ..Uri::default()
        };
        if !reference.scheme.is_empty() || reference.authority.is_some() {
            target.scheme = if reference.scheme.is_empty() {
                self.scheme.clone()
            } else {
                reference.scheme.clone()
            };
            target.authority = reference.authority.clone();
            target.path = resolve_path(&reference.path, "");
            target.query = reference.query.clone();
            return target;
        }
        target.scheme = self.scheme.clone();
        target.authority = self.authority.clone();
        target.query = if reference.path.is_empty() && reference.query.is_none() {
            self.query.clone()
        } else {
            reference.query.clone()
        };
        target.path = resolve_path(&self.path, &reference.path);
        target
    }
}
// Resolves a reference path against a base path: a non-empty result always
// starts with a slash.
fn resolve_path(base: &str, reference: &str) -> String {
    let full = if reference.is_empty() {
        base.to_string()
    } else if !reference.starts_with('/') {
        let directory = base.rfind('/').map_or("", |index| &base[..=index]);
        format!("{directory}{reference}")
    } else {
        reference.to_string()
    };
    if full.is_empty() {
        return String::new();
    }
    let resolved = remove_dots(&full);
    format!("/{}", resolved.trim_start_matches('/'))
}
fn unescape(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let digits = bytes
                .get(index + 1..index + 3)
                .and_then(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok());
            match digits {
                Some(byte) => result.push(byte),
                None => {
                    let end = (index + 3).min(bytes.len());
                    return Err(format!(
                        "invalid URL escape {}",
                        quoted_text(&String::from_utf8_lossy(&bytes[index..end]))
                    ));
                }
            }
            index += 3;
        } else {
            result.push(bytes[index]);
            index += 1;
        }
    }
    Ok(String::from_utf8_lossy(&result).into_owned())
}
fn remove_dots(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let mut output: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').collect();
    let last = segments.len() - 1;
    for (index, segment) in segments.iter().enumerate() {
        match *segment {
            "." => {
                if index == last {
                    output.push("");
                }
            }
            ".." => {
                if output.len() > 1 || (output.len() == 1 && !output[0].is_empty()) {
                    output.pop();
                }
                if index == last {
                    output.push("");
                }
            }
            other => output.push(other),
        }
    }
    let joined = output.join("/");
    if path.starts_with('/') && !joined.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}

fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

enum Step {
    Schema(usize),
    List(Vec<usize>),
    Map(BTreeMap<String, usize>),
    Other(&'static str),
    Missing,
}

impl Resolved {
    /// Decodes a schema and resolves its references.
    pub fn parse(value: &Value) -> Result<Resolved, String> {
        let mut builder = Builder { nodes: Vec::new() };
        let root = builder.schema(value, "")?;
        let mut nodes = builder.nodes;
        // Move the root to index 0, keeping every child index valid.
        let last = nodes.len() - 1;
        debug_assert_eq!(root, last);
        nodes.swap(0, last);
        let swap = |index: &mut usize| {
            if *index == 0 {
                *index = last;
            } else if *index == last {
                *index = 0;
            }
        };
        for node in &mut nodes {
            node.each_child_mut(&swap);
        }
        let mut resolved = Resolved {
            draft7: matches!(nodes[0].schema.as_str(), DRAFT7 | DRAFT7_SECURE),
            nodes,
            uris: BTreeMap::new(),
        };
        resolved.resolve()?;
        Ok(resolved)
    }
    fn children(&self, index: usize) -> Vec<(String, usize)> {
        let node = &self.nodes[index];
        let mut result = Vec::new();
        let one = |name: &str, child: &Option<usize>, result: &mut Vec<(String, usize)>| {
            if let Some(child) = child {
                result.push((format!("/{name}"), *child));
            }
        };
        let many = |name: &str, list: &Option<Vec<usize>>, result: &mut Vec<(String, usize)>| {
            for (position, child) in list.iter().flatten().enumerate() {
                result.push((format!("/{name}/{position}"), *child));
            }
        };
        let keyed = |name: &str,
                     map: &Option<BTreeMap<String, usize>>,
                     result: &mut Vec<(String, usize)>| {
            for (key, child) in map.iter().flatten() {
                result.push((format!("/{name}/{}", escape_pointer(key)), *child));
            }
        };
        // Fields in the order of their JSON names.
        keyed("$defs", &node.defs, &mut result);
        one("additionalItems", &node.additional_items, &mut result);
        one(
            "additionalProperties",
            &node.additional_properties,
            &mut result,
        );
        many("allOf", &node.all_of, &mut result);
        many("anyOf", &node.any_of, &mut result);
        one("contains", &node.contains, &mut result);
        one("contentSchema", &node.content_schema, &mut result);
        keyed("definitions", &node.definitions, &mut result);
        keyed("dependencies", &node.dependency_schemas, &mut result);
        keyed("dependentSchemas", &node.dependent_schemas, &mut result);
        one("else", &node.else_, &mut result);
        one("if", &node.if_, &mut result);
        one("items", &node.items, &mut result);
        many("items", &node.items_array, &mut result);
        one("not", &node.not, &mut result);
        many("oneOf", &node.one_of, &mut result);
        keyed("patternProperties", &node.pattern_properties, &mut result);
        many("prefixItems", &node.prefix_items, &mut result);
        keyed("properties", &node.properties, &mut result);
        one("propertyNames", &node.property_names, &mut result);
        one("then", &node.then, &mut result);
        one("unevaluatedItems", &node.unevaluated_items, &mut result);
        one(
            "unevaluatedProperties",
            &node.unevaluated_properties,
            &mut result,
        );
        result
    }
    // Every node in preorder.
    fn all(&self) -> Vec<usize> {
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut stack = vec![0];
        while let Some(index) = stack.pop() {
            order.push(index);
            for (_, child) in self.children(index).into_iter().rev() {
                stack.push(child);
            }
        }
        order
    }
    // The schema's `$id`, else its anchor, else a placeholder.
    fn display(&self, index: usize) -> String {
        let node = &self.nodes[index];
        if !node.id.is_empty() {
            return node.id.clone();
        }
        let anchor = if node.anchor.is_empty() {
            &node.dynamic_anchor
        } else {
            &node.anchor
        };
        if !anchor.is_empty() {
            return format!("anchor {anchor}");
        }
        "<anonymous schema>".into()
    }
    fn display_list(&self, list: &[usize]) -> String {
        let items: Vec<_> = list.iter().map(|index| self.display(*index)).collect();
        format!("[{}]", items.join(" "))
    }
    // The schema's `$id`, else its path, else a placeholder.
    fn name(&self, index: usize) -> String {
        let node = &self.nodes[index];
        if !node.id.is_empty() {
            node.id.clone()
        } else if !node.path.is_empty() {
            node.path.clone()
        } else {
            "<anonymous schema>".into()
        }
    }
    fn resolve(&mut self) -> Result<(), String> {
        // Check structure: assign paths.
        let mut stack = vec![(0usize, String::new())];
        while let Some((index, path)) = stack.pop() {
            for (suffix, child) in self.children(index) {
                stack.push((child, format!("{path}{suffix}")));
            }
            self.nodes[index].path = if path.is_empty() { "root".into() } else { path };
        }
        // Check each schema locally.
        let order = self.all();
        let mut errors = Vec::new();
        for &index in &order {
            let display = self.display(index);
            let mut report = |message: String| errors.push(format!("schema: {display}: {message}"));
            let node = &self.nodes[index];
            if let (Some(schemas), Some(strings)) =
                (&node.dependency_schemas, &node.dependency_strings)
                && let Some(key) = schemas.keys().find(|key| strings.contains_key(*key))
            {
                errors.push(format!(
                    "dependency key {} cannot be defined as both a schema and a string array",
                    quoted_text(key)
                ));
                continue;
            }
            if node.vocabulary && node.schema != DRAFT2020 {
                report("cannot validate a schema with $vocabulary".into());
            }
            let mut regex = None;
            if !node.pattern.is_empty() {
                match Regex::new(&node.pattern) {
                    Ok(compiled) => regex = Some(compiled),
                    Err(error) => {
                        report(format!("pattern: {}", regex_error(&node.pattern, &error)))
                    }
                }
            }
            let mut pattern_regexes = Vec::new();
            for (pattern, child) in node.pattern_properties.iter().flatten() {
                match Regex::new(pattern) {
                    Ok(compiled) => pattern_regexes.push((compiled, *child)),
                    Err(error) => report(format!(
                        "patternProperties[{}]: {}",
                        quoted_text(pattern),
                        regex_error(pattern, &error)
                    )),
                }
            }
            let node = &mut self.nodes[index];
            node.regex = regex;
            node.pattern_regexes = pattern_regexes;
        }
        if !errors.is_empty() {
            return Err(errors.join("\n"));
        }
        // Resolve the URI of every schema.
        self.nodes[0].uri = Some(Uri::default());
        self.uris.insert(String::new(), 0);
        let mut stack = vec![(0usize, 0usize)];
        while let Some((index, mut base)) = stack.pop() {
            let (id, has_ref, anchor, dynamic_anchor) = {
                let node = &self.nodes[index];
                (
                    node.id.clone(),
                    !node.reference.is_empty(),
                    node.anchor.clone(),
                    node.dynamic_anchor.clone(),
                )
            };
            if !id.is_empty() && !(self.draft7 && has_ref) {
                let id_uri = Uri::parse(&id)?;
                if !self.draft7 && !id_uri.fragment.is_empty() {
                    return Err(format!("$id {id} must not have a fragment"));
                }
                if self.draft7 && !id_uri.fragment.is_empty() {
                    // A duplicate-anchor error is deliberately discarded here.
                    let name = id.strip_prefix('#').unwrap_or(&id).to_string();
                    let _ = self.set_anchor(index, base, &name, false);
                } else {
                    let base_uri = self.nodes[base].uri.clone().unwrap_or_default();
                    let uri = base_uri.resolve(&id_uri);
                    if !uri.absolute() {
                        // A schema that is its own base has already taken the new URI.
                        let shown = if index == base { &uri } else { &base_uri };
                        return Err(format!(
                            "$id {id} does not resolve to an absolute URI (base is {})",
                            quoted_text(&shown.fragmentless())
                        ));
                    }
                    self.uris.insert(uri.fragmentless(), index);
                    self.nodes[index].uri = Some(uri);
                    base = index;
                }
            }
            self.nodes[index].base = base;
            if !self.draft7 {
                let _ = self.set_anchor(index, base, &anchor, false);
                let _ = self.set_anchor(index, base, &dynamic_anchor, true);
            }
            for (_, child) in self.children(index).into_iter().rev() {
                stack.push((child, base));
            }
        }
        // Resolve every reference.
        for &index in &order {
            let (reference, dynamic_ref) = {
                let node = &self.nodes[index];
                (node.reference.clone(), node.dynamic_ref.clone())
            };
            if !reference.is_empty() {
                let (target, _) = self.resolve_ref(index, &reference)?;
                self.nodes[index].resolved_ref = Some(target);
            }
            if !dynamic_ref.is_empty() {
                let (target, fragment) = self.resolve_ref(index, &dynamic_ref)?;
                if fragment.is_empty() {
                    self.nodes[index].resolved_dynamic_ref = Some(target);
                } else {
                    self.nodes[index].dynamic_ref_anchor = fragment;
                }
            }
        }
        Ok(())
    }
    fn set_anchor(
        &mut self,
        index: usize,
        base: usize,
        anchor: &str,
        dynamic: bool,
    ) -> Result<(), String> {
        if anchor.is_empty() {
            return Ok(());
        }
        let uri = self.nodes[base]
            .uri
            .as_ref()
            .map(Uri::fragmentless)
            .unwrap_or_default();
        let anchors = &mut self.nodes[base].anchors;
        if anchors.contains_key(anchor) {
            return Err(format!("duplicate anchor {} in {uri}", quoted_text(anchor)));
        }
        anchors.insert(anchor.to_string(), (index, dynamic));
        Ok(())
    }
    fn resolve_ref(&self, index: usize, reference: &str) -> Result<(usize, String), String> {
        let reference_uri = Uri::parse(reference)?;
        let base = self.nodes[index].base;
        let uri = self.nodes[base]
            .uri
            .clone()
            .unwrap_or_default()
            .resolve(&reference_uri);
        let location = uri.fragmentless();
        let Some(&target) = self.uris.get(&location) else {
            return Err(format!(
                "loading {location}: cannot resolve remote schemas: no loader passed to Schema.Resolve"
            ));
        };
        let fragment = &uri.fragment;
        if !fragment.is_empty() && !fragment.starts_with('/') {
            let Some(&(schema, dynamic)) = self.nodes[target].anchors.get(fragment) else {
                return Err(format!(
                    "no anchor {} in {}",
                    quoted_text(fragment),
                    self.display(index)
                ));
            };
            return Ok((
                schema,
                if dynamic {
                    fragment.clone()
                } else {
                    String::new()
                },
            ));
        }
        self.dereference(target, fragment)
            .map(|schema| (schema, String::new()))
            .map_err(|error| format!("JSON Pointer {}: {error}", quoted_text(fragment)))
    }
    fn field(&self, index: usize, name: &str) -> Step {
        let node = &self.nodes[index];
        let one = |child: &Option<usize>| child.map_or(Step::Other("Schema"), Step::Schema);
        let many = |list: &Option<Vec<usize>>| Step::List(list.clone().unwrap_or_default());
        let keyed =
            |map: &Option<BTreeMap<String, usize>>| Step::Map(map.clone().unwrap_or_default());
        match name {
            "$defs" => keyed(&node.defs),
            "definitions" => keyed(&node.definitions),
            "dependencies" => keyed(&node.dependency_schemas),
            "dependentSchemas" => keyed(&node.dependent_schemas),
            "patternProperties" => keyed(&node.pattern_properties),
            "properties" => keyed(&node.properties),
            "allOf" => many(&node.all_of),
            "anyOf" => many(&node.any_of),
            "oneOf" => many(&node.one_of),
            "prefixItems" => many(&node.prefix_items),
            "items" => match node.items {
                Some(child) => Step::Schema(child),
                None => many(&node.items_array),
            },
            "additionalItems" => one(&node.additional_items),
            "additionalProperties" => one(&node.additional_properties),
            "contains" => one(&node.contains),
            "contentSchema" => one(&node.content_schema),
            "else" => one(&node.else_),
            "if" => one(&node.if_),
            "not" => one(&node.not),
            "propertyNames" => one(&node.property_names),
            "then" => one(&node.then),
            "unevaluatedItems" => one(&node.unevaluated_items),
            "unevaluatedProperties" => one(&node.unevaluated_properties),
            "$id" | "$schema" | "$ref" | "$comment" | "$anchor" | "$dynamicAnchor"
            | "$dynamicRef" | "title" | "description" | "pattern" | "contentEncoding"
            | "contentMediaType" | "format" | "type" => Step::Other("string"),
            "required" => Step::Other("array"),
            "deprecated" | "readOnly" | "writeOnly" | "uniqueItems" => Step::Other("bool"),
            "$vocabulary" | "default" | "examples" | "enum" | "const" | "multipleOf"
            | "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" | "minLength"
            | "maxLength" | "minItems" | "maxItems" | "minContains" | "maxContains"
            | "minProperties" | "maxProperties" | "dependentRequired" => Step::Other("value"),
            _ => Step::Missing,
        }
    }
    // Follows a JSON pointer. Navigation through non-schema fields is refused
    // with a short description of the value.
    fn dereference(&self, root: usize, pointer: &str) -> Result<usize, String> {
        if pointer.is_empty() {
            return Ok(root);
        }
        let mut current = Step::Schema(root);
        for segment in pointer[1..].split('/') {
            let segment = segment.replace("~0", "~").replace("~1", "/");
            current = match current {
                Step::Schema(index) => match self.field(index, &segment) {
                    Step::Missing => {
                        return Err(format!("no schema field {}", quoted_text(&segment)));
                    }
                    step => step,
                },
                Step::List(list) => {
                    if segment == "-" {
                        return Err("the JSON Pointer array segment '-' is not supported".into());
                    }
                    if segment.len() > 1 && segment.starts_with('0') {
                        return Err(format!(
                            "segment {} has leading zeroes",
                            quoted_text(&segment)
                        ));
                    }
                    let position: i64 = segment
                        .parse()
                        .map_err(|_| format!("invalid int: {}", quoted_text(&segment)))?;
                    if position < 0 || position as usize >= list.len() {
                        return Err(format!(
                            "index {position} is out of bounds for array of length {}",
                            list.len()
                        ));
                    }
                    Step::Schema(list[position as usize])
                }
                Step::Map(map) => match map.get(&segment) {
                    Some(child) => Step::Schema(*child),
                    None => return Err(format!("no key {} in map", quoted_text(&segment))),
                },
                Step::Other(kind) => {
                    return Err(format!(
                        "value of type {kind} is not a schema, slice or map"
                    ));
                }
                Step::Missing => unreachable!(),
            };
        }
        match current {
            Step::Schema(index) => Ok(index),
            Step::List(_) => Err("does not refer to a schema, but to a array".into()),
            Step::Map(_) => Err("does not refer to a schema, but to a object".into()),
            Step::Other(kind) => Err(format!("does not refer to a schema, but to a {kind}")),
            Step::Missing => unreachable!(),
        }
    }

    /// Validates an instance: `Ok` or the error text.
    pub fn validate(&self, instance: &Value) -> Result<(), String> {
        let version = &self.nodes[0].schema;
        if !matches!(version.as_str(), "" | DRAFT7 | DRAFT7_SECURE | DRAFT2020) {
            return Err(format!(
                "cannot validate version {version}, supported versions: draft-07 and draft 2020-12"
            ));
        }
        let mut stack = Vec::new();
        self.check(&mut stack, instance, 0, None)
    }
    fn check(
        &self,
        stack: &mut Vec<usize>,
        instance: &Value,
        index: usize,
        caller: Option<&mut Annotations>,
    ) -> Result<(), String> {
        stack.push(index);
        let outcome = self.check_inner(stack, instance, index, caller);
        stack.pop();
        outcome.map_err(|error| format!("validating {}: {error}", self.name(index)))
    }
    fn check_inner(
        &self,
        stack: &mut Vec<usize>,
        instance: &Value,
        index: usize,
        caller: Option<&mut Annotations>,
    ) -> Result<(), String> {
        let schema = &self.nodes[index];
        let mut anns = Annotations::default();
        if !schema.reference.is_empty() {
            let target = schema.resolved_ref.expect("resolved $ref");
            self.check(stack, instance, target, Some(&mut anns))?;
            if self.draft7 {
                return Ok(());
            }
        }
        if !schema.kind.is_empty() || schema.kinds.is_some() {
            let got = json_type(instance);
            if !schema.kind.is_empty() {
                if !(got == schema.kind || got == "integer" && schema.kind == "number") {
                    return Err(format!(
                        "type: {} has type {}, want {}",
                        value_text(instance),
                        quoted_text(got),
                        quoted_text(&schema.kind)
                    ));
                }
            } else if let Some(kinds) = &schema.kinds
                && !(kinds.iter().any(|kind| kind == got)
                    || got == "integer" && kinds.iter().any(|kind| kind == "number"))
            {
                return Err(format!(
                    "type: {} has type {}, want one of {}",
                    value_text(instance),
                    quoted_text(got),
                    quoted_text(&kinds.join(", "))
                ));
            }
        }
        if let Some(options) = &schema.enumeration
            && !options.iter().any(|option| equal(option, instance))
        {
            return Err(format!(
                "enum: {} does not equal any of: {}",
                value_text(instance),
                nested_text(&Value::Array(options.clone()))
            ));
        }
        if let Some(constant) = &schema.constant
            && !equal(constant, instance)
        {
            return Err(format!(
                "const: {} does not equal {}",
                value_text(instance),
                nested_text(constant)
            ));
        }
        if let Some(number) = instance.as_f64().filter(|_| instance.is_number()) {
            let rat = || rat_string(number);
            if let Some(multiple) = schema.multiple_of
                && (number / multiple).fract() != 0.0
            {
                return Err(format!(
                    "multipleOf: {} is not a multiple of {multiple:.6}",
                    rat()
                ));
            }
            if let Some(minimum) = schema.minimum
                && number < minimum
            {
                return Err(format!("minimum: {} is less than {minimum:.6}", rat()));
            }
            if let Some(maximum) = schema.maximum
                && number > maximum
            {
                return Err(format!("maximum: {} is greater than {maximum:.6}", rat()));
            }
            if let Some(minimum) = schema.exclusive_minimum
                && number <= minimum
            {
                return Err(format!(
                    "exclusiveMinimum: {} is less than or equal to {minimum:.6}",
                    rat()
                ));
            }
            if let Some(maximum) = schema.exclusive_maximum
                && number >= maximum
            {
                return Err(format!(
                    "exclusiveMaximum: {} is greater than or equal to {maximum:.6}",
                    rat()
                ));
            }
        }
        if let Value::String(text) = instance {
            let count = text.chars().count() as i64;
            if let Some(minimum) = schema.min_length
                && count < minimum
            {
                return Err(format!(
                    "minLength: {} contains {count} Unicode code points, fewer than {minimum}",
                    quoted_text(text)
                ));
            }
            if let Some(maximum) = schema.max_length
                && count > maximum
            {
                return Err(format!(
                    "maxLength: {} contains {count} Unicode code points, more than {maximum}",
                    quoted_text(text)
                ));
            }
            if let Some(regex) = &schema.regex
                && !regex.is_match(text)
            {
                return Err(format!(
                    "pattern: {} does not match regular expression {}",
                    quoted_text(text),
                    quoted_text(&schema.pattern)
                ));
            }
        }
        if !schema.dynamic_ref.is_empty() {
            let target = match schema.resolved_dynamic_ref {
                Some(target) => target,
                None => stack
                    .iter()
                    .find_map(|entry| {
                        let base = self.nodes[*entry].base;
                        self.nodes[base]
                            .anchors
                            .get(&schema.dynamic_ref_anchor)
                            .filter(|(_, dynamic)| *dynamic)
                            .map(|(target, _)| *target)
                    })
                    .ok_or_else(|| {
                        format!(
                            "missing dynamic anchor {}",
                            quoted_text(&schema.dynamic_ref_anchor)
                        )
                    })?,
            };
            self.check(stack, instance, target, Some(&mut anns))?;
        }
        if let Some(all) = &schema.all_of {
            for child in all {
                self.check(stack, instance, *child, Some(&mut anns))?;
            }
        }
        if let Some(any) = &schema.any_of {
            let mut errors = Vec::new();
            for child in any {
                if let Err(error) = self.check(stack, instance, *child, Some(&mut anns)) {
                    errors.push(error);
                }
            }
            if errors.len() == any.len() {
                return Err(format!(
                    "anyOf: did not validate against any of {}:\n{}",
                    self.display_list(any),
                    errors.join("\n")
                ));
            }
        }
        if let Some(one) = &schema.one_of {
            let mut matched: Option<usize> = None;
            for child in one {
                if self.check(stack, instance, *child, Some(&mut anns)).is_ok() {
                    if let Some(previous) = matched {
                        return Err(format!(
                            "oneOf: validated against both {} and {}",
                            self.display(previous),
                            self.display(*child)
                        ));
                    }
                    matched = Some(*child);
                }
            }
            if matched.is_none() {
                return Err(format!(
                    "oneOf: did not validate against any of {}",
                    self.display_list(one)
                ));
            }
        }
        if let Some(not) = schema.not
            && self.check(stack, instance, not, None).is_ok()
        {
            return Err(format!("not: validated against {}", self.display(not)));
        }
        if let Some(condition) = schema.if_ {
            let branch = if self
                .check(stack, instance, condition, Some(&mut anns))
                .is_ok()
            {
                schema.then
            } else {
                schema.else_
            };
            if let Some(branch) = branch {
                self.check(stack, instance, branch, Some(&mut anns))?;
            }
        }
        if let Value::Array(items) = instance {
            if self.draft7 {
                if let Some(schemas) = &schema.items_array {
                    for (item, child) in items.iter().zip(schemas) {
                        self.check(stack, item, *child, None)?;
                    }
                    anns.note_end(schemas.len().min(items.len()));
                    if let Some(additional) = schema.additional_items {
                        for item in items.iter().skip(schemas.len()) {
                            self.check(stack, item, additional, None)?;
                        }
                        anns.all_items = true;
                    }
                } else if let Some(child) = schema.items {
                    for item in items {
                        self.check(stack, item, child, None)?;
                    }
                    anns.all_items = true;
                }
            } else {
                let prefix = schema.prefix_items.as_deref().unwrap_or_default();
                for (item, child) in items.iter().zip(prefix) {
                    self.check(stack, item, *child, None)?;
                }
                anns.note_end(prefix.len().min(items.len()));
                if let Some(child) = schema.items {
                    for item in items.iter().skip(prefix.len()) {
                        self.check(stack, item, child, None)?;
                    }
                    anns.all_items = true;
                }
            }
            let mut contained = 0i64;
            if let Some(contains) = schema.contains {
                for (position, item) in items.iter().enumerate() {
                    if self.check(stack, item, contains, None).is_ok() {
                        contained += 1;
                        anns.indexes.insert(position);
                    }
                }
                if contained == 0 && schema.min_contains.is_none_or(|minimum| minimum > 0) {
                    return Err(format!(
                        "contains: {} does not have an item matching {}",
                        string_verb(instance),
                        self.display(contains)
                    ));
                }
                if let Some(minimum) = schema.min_contains
                    && contained < minimum
                {
                    return Err(format!(
                        "minContains: contains validated {contained} items, less than {minimum}"
                    ));
                }
                if let Some(maximum) = schema.max_contains
                    && contained > maximum
                {
                    return Err(format!(
                        "maxContains: contains validated {contained} items, greater than {maximum}"
                    ));
                }
            }
            let length = items.len() as i64;
            if let Some(minimum) = schema.min_items
                && length < minimum
            {
                return Err(format!(
                    "minItems: array length {length} is less than {minimum}"
                ));
            }
            if let Some(maximum) = schema.max_items
                && length > maximum
            {
                return Err(format!(
                    "maxItems: array length {length} is greater than {maximum}"
                ));
            }
            if schema.unique_items {
                for later in 1..items.len() {
                    for earlier in 0..later {
                        if equal(&items[later], &items[earlier]) {
                            return Err(format!(
                                "uniqueItems: array items {later} and {earlier} are equal"
                            ));
                        }
                    }
                }
            }
            if let Some(unevaluated) = schema.unevaluated_items
                && !anns.all_items
            {
                for (position, item) in items.iter().enumerate().skip(anns.end_index) {
                    if !anns.indexes.contains(&position) {
                        self.check(stack, item, unevaluated, None)?;
                    }
                }
                anns.all_items = true;
            }
        }
        if let Value::Object(members) = instance {
            let mut evaluated = BTreeSet::new();
            for (property, child) in schema.properties.iter().flatten() {
                if let Some(value) = members.get(property) {
                    self.check(stack, value, *child, None)?;
                    evaluated.insert(property.clone());
                }
            }
            if !schema.pattern_regexes.is_empty() {
                for (property, value) in members {
                    for (regex, child) in &schema.pattern_regexes {
                        if regex.is_match(property) {
                            self.check(stack, value, *child, None)?;
                            evaluated.insert(property.clone());
                        }
                    }
                }
            }
            if let Some(additional) = schema.additional_properties {
                let falsy = self.nodes[additional]
                    .not
                    .is_some_and(|not| self.nodes[not].zero);
                if falsy {
                    let disallowed: Vec<_> = members
                        .keys()
                        .filter(|property| !evaluated.contains(*property))
                        .map(|property| quoted_text(property))
                        .collect();
                    if !disallowed.is_empty() {
                        return Err(format!(
                            "unexpected additional properties [{}]",
                            disallowed.join(" ")
                        ));
                    }
                } else {
                    for (property, value) in members {
                        if !evaluated.contains(property) {
                            self.check(stack, value, additional, None)?;
                            evaluated.insert(property.clone());
                        }
                    }
                }
            }
            anns.properties.extend(evaluated);
            if let Some(names) = schema.property_names {
                for property in members.keys() {
                    self.check(stack, &Value::String(property.clone()), names, None)?;
                }
            }
            let count = members.len() as i64;
            if let Some(minimum) = schema.min_properties
                && count < minimum
            {
                return Err(format!(
                    "minProperties: object has {count} properties, less than {minimum}"
                ));
            }
            if let Some(maximum) = schema.max_properties
                && count > maximum
            {
                return Err(format!(
                    "maxProperties: object has {count} properties, greater than {maximum}"
                ));
            }
            let missing = |names: &[String]| -> Vec<String> {
                names
                    .iter()
                    .filter(|name| !members.contains_key(*name))
                    .map(|name| quoted_text(name))
                    .collect()
            };
            if let Some(required) = &schema.required {
                let absent = missing(required);
                if !absent.is_empty() {
                    return Err(format!(
                        "required: missing properties: [{}]",
                        absent.join(" ")
                    ));
                }
            }
            let (strings, schemas) = if self.draft7 {
                (&schema.dependency_strings, &schema.dependency_schemas)
            } else {
                (&schema.dependent_required, &schema.dependent_schemas)
            };
            for (property, names) in strings.iter().flatten() {
                if members.contains_key(property) {
                    let absent = missing(names);
                    if !absent.is_empty() {
                        return Err(format!(
                            "dependentRequired[{}]: missing properties [{}]",
                            quoted_text(property),
                            absent.join(" ")
                        ));
                    }
                }
            }
            for (property, child) in schemas.iter().flatten() {
                if members.contains_key(property) {
                    self.check(stack, instance, *child, Some(&mut anns))?;
                }
            }
            if let Some(unevaluated) = schema.unevaluated_properties
                && !anns.all_properties
            {
                for (property, value) in members {
                    if !anns.properties.contains(property) {
                        self.check(stack, value, unevaluated, None)?;
                    }
                }
                anns.all_properties = true;
            }
        }
        if let Some(caller) = caller {
            caller.merge(anns);
        }
        Ok(())
    }
}

impl Node {
    fn each_child_mut(&mut self, action: &impl Fn(&mut usize)) {
        for child in [
            &mut self.items,
            &mut self.additional_items,
            &mut self.contains,
            &mut self.unevaluated_items,
            &mut self.additional_properties,
            &mut self.property_names,
            &mut self.unevaluated_properties,
            &mut self.not,
            &mut self.if_,
            &mut self.then,
            &mut self.else_,
            &mut self.content_schema,
        ]
        .into_iter()
        .flatten()
        {
            action(child);
        }
        for list in [
            &mut self.prefix_items,
            &mut self.items_array,
            &mut self.all_of,
            &mut self.any_of,
            &mut self.one_of,
        ]
        .into_iter()
        .flatten()
        {
            list.iter_mut().for_each(action);
        }
        for map in [
            &mut self.properties,
            &mut self.pattern_properties,
            &mut self.dependent_schemas,
            &mut self.dependency_schemas,
            &mut self.defs,
            &mut self.definitions,
        ]
        .into_iter()
        .flatten()
        {
            map.values_mut().for_each(action);
        }
    }
}

#[derive(Default)]
struct Annotations {
    all_items: bool,
    end_index: usize,
    indexes: BTreeSet<usize>,
    all_properties: bool,
    properties: BTreeSet<String>,
}
impl Annotations {
    fn note_end(&mut self, end: usize) {
        self.end_index = self.end_index.max(end);
    }
    fn merge(&mut self, other: Annotations) {
        self.all_items |= other.all_items;
        self.end_index = self.end_index.max(other.end_index);
        self.indexes.extend(other.indexes);
        self.all_properties |= other.all_properties;
        self.properties.extend(other.properties);
    }
}

fn regex_error(pattern: &str, error: &regex::Error) -> String {
    let reason = match error {
        regex::Error::Syntax(text) => text
            .lines()
            .last()
            .unwrap_or_default()
            .trim_start_matches("error: ")
            .to_string(),
        other => other.to_string(),
    };
    format!("error parsing regexp: {reason}: `{pattern}`")
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) => {
            if number.as_f64().is_some_and(|float| float.fract() == 0.0) {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
// Deep equality: numbers compare by value.
fn equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| equal(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, x)| b.get(key).is_some_and(|y| equal(x, y)))
        }
        (a, b) => a == b,
    }
}

/// Shortest round-trip text of an f64, in scientific form (`1e+06`) when the
/// decimal exponent is below -4 or at least 6.
pub fn float_layout(number: f64) -> String {
    if number.is_nan() {
        return "NaN".into();
    }
    if number.is_infinite() {
        return if number > 0.0 { "+Inf" } else { "-Inf" }.into();
    }
    if number == 0.0 {
        return if number.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let scientific = format!("{number:e}");
    let (mantissa, exponent) = scientific.split_once('e').expect("exponent form");
    let exponent: i32 = exponent.parse().expect("exponent");
    if !(-4..6).contains(&exponent) {
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }
    format!("{number}")
}
fn nested_text(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".into(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => float_layout(number.as_f64().unwrap_or(f64::NAN)),
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            let items: Vec<_> = items.iter().map(nested_text).collect();
            format!("[{}]", items.join(" "))
        }
        Value::Object(members) => map_text(members, nested_text),
    }
}
fn map_text(members: &Map<String, Value>, format: fn(&Value) -> String) -> String {
    let items: Vec<_> = members
        .iter()
        .map(|(key, value)| format!("{key}:{}", format(value)))
        .collect();
    format!("map[{}]", items.join(" "))
}
/// An instance as shown in error messages; `null` has a fixed placeholder text.
fn value_text(value: &Value) -> String {
    match value {
        Value::Null => "<invalid reflect.Value>".into(),
        other => nested_text(other),
    }
}
// An instance shown where a string is expected: non-strings carry a
// `%!s(type=value)` marker.
fn string_verb(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".into(),
        Value::Bool(flag) => format!("%!s(bool={flag})"),
        Value::Number(number) => {
            format!(
                "%!s(float64={})",
                float_layout(number.as_f64().unwrap_or(f64::NAN))
            )
        }
        Value::String(text) => text.clone(),
        Value::Array(items) => {
            let items: Vec<_> = items.iter().map(string_verb).collect();
            format!("[{}]", items.join(" "))
        }
        Value::Object(members) => map_text(members, string_verb),
    }
}

// Decimal text of `mantissa * 2^shift`.
fn shifted(mantissa: u64, shift: u32) -> String {
    const BASE: u64 = 1_000_000_000;
    let mut limbs = vec![
        mantissa % BASE,
        (mantissa / BASE) % BASE,
        mantissa / BASE / BASE,
    ];
    for _ in 0..shift {
        let mut carry = 0;
        for limb in &mut limbs {
            let doubled = *limb * 2 + carry;
            *limb = doubled % BASE;
            carry = doubled / BASE;
        }
        if carry != 0 {
            limbs.push(carry);
        }
    }
    while limbs.len() > 1 && limbs.last() == Some(&0) {
        limbs.pop();
    }
    let mut text = limbs.last().expect("limb").to_string();
    for limb in limbs.iter().rev().skip(1) {
        text.push_str(&format!("{limb:09}"));
    }
    text
}
/// The exact value of an f64 as `numerator/denominator` in lowest terms.
fn rat_string(number: f64) -> String {
    if number == 0.0 {
        return "0/1".into();
    }
    let bits = number.to_bits();
    let sign = if bits >> 63 == 1 { "-" } else { "" };
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1 << 52) - 1);
    let (mut mantissa, mut exponent) = if biased == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), biased - 1075)
    };
    if exponent < 0 {
        let reduce = (mantissa.trailing_zeros() as i32).min(-exponent);
        mantissa >>= reduce;
        exponent += reduce;
    }
    if exponent >= 0 {
        format!("{sign}{}/1", shifted(mantissa, exponent as u32))
    } else {
        format!("{sign}{mantissa}/{}", shifted(1, (-exponent) as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn outcome(schema: Value, instance: Value) -> String {
        match Resolved::parse(&schema) {
            Err(error) => format!("resolve: {error}"),
            Ok(resolved) => resolved.validate(&instance).err().unwrap_or_default(),
        }
    }
    #[test]
    fn number_and_rational_text() {
        for (number, text) in [
            (5.0, "5"),
            (1.5, "1.5"),
            (1e21, "1e+21"),
            (1e20, "1e+20"),
            (1000000.0, "1e+06"),
            (123456.0, "123456"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (9223372036854775807.0, "9.223372036854776e+18"),
        ] {
            assert_eq!(float_layout(number), text);
        }
        assert_eq!(rat_string(0.0), "0/1");
        assert_eq!(rat_string(1.5), "3/2");
        assert_eq!(rat_string(-4.0), "-4/1");
        assert_eq!(rat_string(0.1), "3602879701896397/36028797018963968");
        assert_eq!(rat_string(1e21), "1000000000000000000000/1");
    }
    #[test]
    fn messages_are_fixed() {
        let object = json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false});
        assert_eq!(
            outcome(object.clone(), json!({"path":"a","extra":1})),
            "validating root: unexpected additional properties [\"extra\"]"
        );
        assert_eq!(
            outcome(object.clone(), json!({})),
            "validating root: required: missing properties: [\"path\"]"
        );
        assert_eq!(
            outcome(object.clone(), json!({"path":5})),
            "validating root: validating /properties/path: type: 5 has type \"integer\", want \"string\""
        );
        assert_eq!(
            outcome(object.clone(), json!({"path":"a","offset":0})),
            "validating root: validating /properties/offset: minimum: 0/1 is less than 1.000000"
        );
        assert_eq!(
            outcome(object, json!([1])),
            "validating root: type: [1] has type \"array\", want \"object\""
        );
        assert_eq!(
            outcome(
                json!({"type":"object","$ref":"https://invalid.example/schema"}),
                json!({})
            ),
            "resolve: loading https://invalid.example/schema: cannot resolve remote schemas: no loader passed to Schema.Resolve"
        );
        assert_eq!(
            outcome(
                json!({"oneOf":[{"required":["path"]},{"required":["fileId"]}]}),
                json!({"path":"a","fileId":"b"})
            ),
            "validating root: oneOf: validated against both <anonymous schema> and <anonymous schema>"
        );
    }
}
