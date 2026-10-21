//! The subset of YAML 1.1 reading and emitting that determines authored bytes: scalar
//! resolution, map key order, node encoding and the emitter. Comments, anchors and
//! explicit tags are not re-encoded; reusing a source node that carries one fails
//! explicitly instead of changing bytes.
use super::store::{AuthoringError, Result, failure};
use regex::Regex;
use serde_json::{Map, Number, Value};
use std::sync::LazyLock;
use yaml_rust2::{
    parser::{Event, Parser, Tag},
    scanner::TScalarStyle,
};

pub(crate) const STR: &str = "!!str";
pub const INT: &str = "!!int";
pub const FLOAT: &str = "!!float";
pub const NULL: &str = "!!null";
pub(crate) const BOOL: &str = "!!bool";
pub(crate) const TIMESTAMP: &str = "!!timestamp";
pub const MERGE: &str = "!!merge";
pub(crate) const SEQ: &str = "!!seq";
pub(crate) const MAP: &str = "!!map";
const LONG_PREFIX: &str = "tag:yaml.org,2002:";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Scalar,
    Mapping,
    Sequence,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    Single,
    Double,
    Literal,
    Folded,
}
/// A YAML node with byte offsets instead of line/column pairs.
#[derive(Clone, Debug)]
pub struct Node {
    pub kind: Kind,
    pub style: Style,
    pub flow: bool,
    pub tag: String,
    /// Explicit source tag or anchor; never re-encoded.
    pub tagged: bool,
    pub anchored: bool,
    pub value: String,
    pub content: Vec<Node>,
    pub start: usize,
    /// Implicit empty plain scalar; it is placed right after its indicator.
    pub empty: bool,
    /// Pre-order position and the last position inside this subtree.
    pub index: usize,
    pub last: usize,
}
impl Node {
    fn new(kind: Kind, tag: &str) -> Self {
        Self {
            kind,
            style: Style::Plain,
            flow: false,
            tag: tag.into(),
            tagged: false,
            anchored: false,
            value: String::new(),
            content: Vec::new(),
            start: 0,
            empty: false,
            index: 0,
            last: 0,
        }
    }
    pub fn scalar(value: &str, tag: &str, style: Style) -> Self {
        let mut node = Self::new(Kind::Scalar, tag);
        node.value = value.into();
        node.style = style;
        node
    }
    /// The value node of a mapping key and the index of that key.
    pub fn field(&self, name: &str) -> Option<(&Node, usize)> {
        if self.kind != Kind::Mapping {
            return None;
        }
        (0..self.content.len() / 2)
            .map(|pair| pair * 2)
            .find(|&index| self.content[index].value == name)
            .map(|index| (&self.content[index + 1], index))
    }
    pub fn visit<'a>(&'a self, action: &mut impl FnMut(&'a Node)) {
        action(self);
        for child in &self.content {
            child.visit(action);
        }
    }
}
fn invalid(error: impl std::fmt::Display) -> AuthoringError {
    failure("invalid_yaml", error.to_string(), None)
}
pub(crate) fn internal(message: impl Into<String>) -> AuthoringError {
    failure("internal_error", message, None)
}
pub(crate) fn unported_comment() -> AuthoringError {
    unported("a comment")
}
fn unported(what: &str) -> AuthoringError {
    failure(
        "unsupported_yaml_reencode",
        format!("re-encoding a source YAML node with {what} is not supported"),
        None,
    )
}

// ---- number text ----

fn scalar_uint(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let (radix, digits) = if bytes[0] == b'0' {
        match bytes.get(1).map(u8::to_ascii_lowercase) {
            Some(b'b') if bytes.len() >= 3 => (2, &text[2..]),
            Some(b'o') if bytes.len() >= 3 => (8, &text[2..]),
            Some(b'x') if bytes.len() >= 3 => (16, &text[2..]),
            _ => (8, &text[1..]),
        }
    } else {
        (10, text)
    };
    if digits.is_empty() {
        // Only "0" reaches this: its octal prefix leaves no digits.
        return Some(0);
    }
    if !digits.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}
/// Signed 64-bit integer with base prefixes (`0x`, `0b`, `0o`, leading `0`), for text without underscores.
fn scalar_int(text: &str) -> Option<i64> {
    let (negative, digits) = match text.as_bytes().first()? {
        b'+' => (false, &text[1..]),
        b'-' => (true, &text[1..]),
        _ => (false, text),
    };
    let value = scalar_uint(digits)?;
    if negative {
        if value > 1 << 63 {
            return None;
        }
        Some((value as i64).wrapping_neg())
    } else {
        i64::try_from(value).ok()
    }
}
/// A finite f64 from the decimal forms that scalar resolution accepts.
fn scalar_float(text: &str) -> Option<f64> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty()
        || !digits
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        || !digits.bytes().any(|b| b.is_ascii_digit())
    {
        return None;
    }
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}
fn decimal(value: f64) -> (Vec<u8>, i32) {
    // Shortest round-trip digits and the decimal point position.
    if value == 0.0 {
        return (Vec::new(), 0);
    }
    let text = format!("{:e}", value.abs());
    let (mantissa, exponent) = text.split_once('e').unwrap();
    let digits: Vec<u8> = mantissa.bytes().filter(|b| *b != b'.').collect();
    (digits, exponent.parse::<i32>().unwrap() + 1)
}
fn format_e(negative: bool, digits: &[u8], point: i32) -> String {
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push(digits.first().map_or('0', |d| *d as char));
    if digits.len() > 1 {
        out.push('.');
        out.push_str(std::str::from_utf8(&digits[1..]).unwrap());
    }
    let exponent = if digits.is_empty() { 0 } else { point - 1 };
    out.push('e');
    out.push(if exponent < 0 { '-' } else { '+' });
    out.push_str(&format!("{:02}", exponent.abs()));
    out
}
fn format_f(negative: bool, digits: &[u8], point: i32) -> String {
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    let count = digits.len() as i32;
    if point > 0 {
        for index in 0..point {
            out.push(if index < count {
                digits[index as usize] as char
            } else {
                '0'
            });
        }
    } else {
        out.push('0');
    }
    if count > point {
        out.push('.');
        for index in point..count {
            out.push(if index >= 0 {
                digits[index as usize] as char
            } else {
                '0'
            });
        }
    }
    out
}
/// Shortest round-trip text of an f64, in exponent form (`1e+06`) when the decimal
/// exponent is below -4 or at least 6; also the display text of an f64.
pub(crate) fn format_g(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "+Inf" } else { "-Inf" }.into();
    }
    let (digits, point) = decimal(value);
    let exponent = point - 1;
    if !(-4..6).contains(&exponent) {
        format_e(value.is_sign_negative(), &digits, point)
    } else {
        format_f(value.is_sign_negative(), &digits, point)
    }
}
/// JSON text of an f64: shortest round-trip form, exponent form outside [1e-6, 1e21).
pub(crate) fn format_json(value: f64) -> String {
    let (digits, point) = decimal(value);
    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-6..1e21).contains(&magnitude) {
        let mut text = format_e(value.is_sign_negative(), &digits, point);
        let bytes = text.as_bytes();
        let n = bytes.len();
        if n >= 4 && bytes[n - 4] == b'e' && bytes[n - 2] == b'0' {
            text.remove(n - 2);
        }
        text
    } else {
        format_f(value.is_sign_negative(), &digits, point)
    }
}
/// Numbers produced from an f64 carry a fraction or exponent; integers never do.
pub(crate) fn is_float(number: &Number) -> bool {
    number.to_string().contains(['.', 'e', 'E'])
}
pub(crate) fn float_number(value: f64) -> Result<Number> {
    Number::from_f64(value)
        .ok_or_else(|| internal(format!("json: unsupported value: {}", format_g(value))))
}
fn number_text(number: &Number) -> String {
    let text = number.to_string();
    if is_float(number) {
        text.parse::<f64>().map_or(text, format_g)
    } else {
        text
    }
}

// ---- scalar resolution ----

static YAML_FLOAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$").unwrap());
static BASE60: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+(?:\.[0-9_]*)?$").unwrap());
#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    /// A timestamp in the RFC 3339 text JSON prints for it.
    Timestamp(String),
    Str,
}
impl Resolved {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Null => NULL,
            Self::Bool(_) => BOOL,
            Self::Int(_) | Self::Uint(_) => INT,
            Self::Float(_) => FLOAT,
            Self::Timestamp(_) => TIMESTAMP,
            Self::Str => STR,
        }
    }
}
fn number(text: &[u8], position: &mut usize, min: usize, max: usize) -> Option<u32> {
    let start = *position;
    while *position < text.len() && *position - start < max && text[*position].is_ascii_digit() {
        *position += 1;
    }
    if *position - start < min {
        return None;
    }
    std::str::from_utf8(&text[start..*position])
        .ok()?
        .parse()
        .ok()
}
fn literal(text: &[u8], position: &mut usize, expected: u8) -> Option<()> {
    (text.get(*position) == Some(&expected)).then(|| *position += 1)
}
/// A YAML timestamp (a date, or a date and time with optional fraction and zone)
/// as RFC 3339 text; `None` when the text is not one.
fn parse_timestamp(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits != 4 || bytes.get(4) != Some(&b'-') {
        return None;
    }
    let mut at = 0;
    let year = number(bytes, &mut at, 4, 4)?;
    literal(bytes, &mut at, b'-')?;
    let month = number(bytes, &mut at, 1, 2)?;
    literal(bytes, &mut at, b'-')?;
    let day = number(bytes, &mut at, 1, 2)?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day < 1 || day > days {
        return None;
    }
    let date = format!("{year:04}-{month:02}-{day:02}");
    if at == bytes.len() {
        return Some(format!("{date}T00:00:00Z"));
    }
    let separator = bytes[at];
    if !matches!(separator, b'T' | b't' | b' ') {
        return None;
    }
    at += 1;
    let hour = number(bytes, &mut at, 1, 2)?;
    literal(bytes, &mut at, b':')?;
    let minute = number(bytes, &mut at, 1, 2)?;
    literal(bytes, &mut at, b':')?;
    let second = number(bytes, &mut at, 1, 2)?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut fraction = String::new();
    if matches!(bytes.get(at), Some(b'.' | b','))
        && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)
    {
        at += 1;
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        let digits = &text[start..at.min(start + 9)];
        let trimmed = digits.trim_end_matches('0');
        if !trimmed.is_empty() {
            fraction = format!(".{trimmed}");
        }
    }
    let time = format!("{date}T{hour:02}:{minute:02}:{second:02}{fraction}");
    if separator == b' ' {
        return (at == bytes.len()).then(|| format!("{time}Z"));
    }
    if bytes.get(at) == Some(&b'Z') {
        return (at + 1 == bytes.len()).then(|| format!("{time}Z"));
    }
    let sign = *bytes.get(at)?;
    if !matches!(sign, b'+' | b'-') || bytes.len() != at + 6 {
        return None;
    }
    at += 1;
    let zone_hour = number(bytes, &mut at, 2, 2)?;
    literal(bytes, &mut at, b':')?;
    let zone_minute = number(bytes, &mut at, 2, 2)?;
    if zone_hour > 23 || zone_minute > 59 {
        return None;
    }
    if zone_hour == 0 && zone_minute == 0 {
        return Some(format!("{time}Z"));
    }
    Some(format!(
        "{time}{}{zone_hour:02}:{zone_minute:02}",
        sign as char
    ))
}
/// The type of an untagged plain scalar.
pub fn resolve_value(text: &str) -> Resolved {
    let Some(&first) = text.as_bytes().first() else {
        return Resolved::Null;
    };
    let hint = match first {
        b'+' | b'-' => b'S',
        b'0'..=b'9' => b'D',
        b'y' | b'Y' | b'n' | b'N' | b't' | b'T' | b'f' | b'F' | b'o' | b'O' | b'~' => b'M',
        b'.' => b'.',
        _ => return Resolved::Str,
    };
    match text {
        "true" | "True" | "TRUE" => return Resolved::Bool(true),
        "false" | "False" | "FALSE" => return Resolved::Bool(false),
        "~" | "null" | "Null" | "NULL" => return Resolved::Null,
        ".nan" | ".NaN" | ".NAN" => return Resolved::Float(f64::NAN),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            return Resolved::Float(f64::INFINITY);
        }
        "-.inf" | "-.Inf" | "-.INF" => return Resolved::Float(f64::NEG_INFINITY),
        _ => {}
    }
    match hint {
        b'.' => {
            if let Some(value) = scalar_float(text) {
                return Resolved::Float(value);
            }
        }
        b'D' | b'S' => {
            if let Some(time) = parse_timestamp(text) {
                return Resolved::Timestamp(time);
            }
            let plain = text.replace('_', "");
            if let Some(value) = scalar_int(&plain) {
                return Resolved::Int(value);
            }
            if let Some(value) = scalar_uint(&plain) {
                return Resolved::Uint(value);
            }
            if YAML_FLOAT.is_match(&plain)
                && let Some(value) = scalar_float(&plain)
            {
                return Resolved::Float(value);
            }
        }
        _ => {}
    }
    Resolved::Str
}
pub(crate) fn resolve(text: &str) -> &'static str {
    resolve_value(text).tag()
}
fn is_base60(text: &str) -> bool {
    let Some(&first) = text.as_bytes().first() else {
        return false;
    };
    (matches!(first, b'+' | b'-') || first.is_ascii_digit())
        && text.contains(':')
        && BASE60.is_match(text)
}
fn is_old_bool(text: &str) -> bool {
    matches!(
        text,
        "y" | "Y"
            | "yes"
            | "Yes"
            | "YES"
            | "on"
            | "On"
            | "ON"
            | "n"
            | "N"
            | "no"
            | "No"
            | "NO"
            | "off"
            | "Off"
            | "OFF"
    )
}

// ---- key order ----

fn is_digit(c: char) -> bool {
    c.is_ascii_digit() || (!c.is_ascii() && c.is_numeric())
}
fn key_less(a: &str, b: &str) -> bool {
    let (ar, br): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut digits = false;
    for i in 0..ar.len().min(br.len()) {
        if ar[i] == br[i] {
            digits = is_digit(ar[i]);
            continue;
        }
        let (al, bl) = (ar[i].is_alphabetic(), br[i].is_alphabetic());
        if al && bl {
            return ar[i] < br[i];
        }
        if al || bl {
            return if digits { al } else { bl };
        }
        let (mut an, mut bn) = (0i64, 0i64);
        if ar[i] == '0' || br[i] == '0' {
            let mut j = i;
            while j > 0 && is_digit(ar[j - 1]) {
                j -= 1;
                if ar[j] != '0' {
                    an = 1;
                    bn = 1;
                    break;
                }
            }
        }
        let (mut ai, mut bi) = (i, i);
        while ai < ar.len() && is_digit(ar[ai]) {
            an = an.wrapping_mul(10).wrapping_add(ar[ai] as i64 - 48);
            ai += 1;
        }
        while bi < br.len() && is_digit(br[bi]) {
            bn = bn.wrapping_mul(10).wrapping_add(br[bi] as i64 - 48);
            bi += 1;
        }
        if an != bn {
            return an < bn;
        }
        if ai != bi {
            return ai < bi;
        }
        return ar[i] < br[i];
    }
    ar.len() < br.len()
}
/// Map keys in the order the emitter writes them.
pub(crate) fn sorted_keys(map: &Map<String, Value>) -> Vec<&String> {
    let mut keys: Vec<&String> = Vec::with_capacity(map.len());
    // The comparison is not guaranteed to be a total order; insert stably.
    for key in map.keys() {
        let position = keys
            .iter()
            .position(|existing| key_less(key, existing))
            .unwrap_or(keys.len());
        keys.insert(position, key);
    }
    keys
}

// ---- parsing to nodes ----

fn short_tag(tag: &Tag) -> String {
    let full = format!("{}{}", tag.handle, tag.suffix);
    match full.strip_prefix(LONG_PREFIX) {
        Some(suffix) => format!("!!{suffix}"),
        None => full,
    }
}
fn dash_before(bytes: &[u8], position: usize) -> usize {
    let mut index = position;
    while index > 0 && matches!(bytes[index - 1], b' ' | b'\t' | b'\r' | b'\n') {
        index -= 1;
    }
    if index > 0 && bytes[index - 1] == b'-' {
        index - 1
    } else {
        position
    }
}
fn quoted_end(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = *bytes.get(start)?;
    let mut index = start + 1;
    while index < bytes.len() {
        if quote == b'"' && bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == quote {
            if quote == b'\'' && bytes.get(index + 1) == Some(&quote) {
                index += 2;
                continue;
            }
            return Some(index + 1);
        }
        index += 1;
    }
    None
}
fn after_key(bytes: &[u8], key: &Node) -> usize {
    let end = match key.style {
        Style::Single | Style::Double => quoted_end(bytes, key.start),
        _ => bytes[key.start.min(bytes.len())..]
            .starts_with(key.value.as_bytes())
            .then(|| key.start + key.value.len()),
    };
    let Some(mut index) = end else {
        return key.start;
    };
    while matches!(bytes.get(index), Some(b' ' | b'\t')) {
        index += 1;
    }
    if bytes.get(index) == Some(&b':') {
        index + 1
    } else {
        key.start
    }
}
fn finish(node: &mut Node, bytes: &[u8]) {
    let first = node.content.first().map(|child| child.start);
    match node.kind {
        Kind::Mapping => {
            // yaml-rust2 marks a block mapping at its first value indicator.
            node.flow = bytes.get(node.start) == Some(&b'{');
            if !node.flow
                && let Some(first) = first
            {
                node.start = first;
            }
            for pair in 0..node.content.len() / 2 {
                if node.content[pair * 2 + 1].empty {
                    node.content[pair * 2 + 1].start = after_key(bytes, &node.content[pair * 2]);
                }
            }
        }
        Kind::Sequence => match first {
            None => node.flow = true,
            // An indentless sequence is marked at its first entry's value.
            Some(first) if first == node.start => node.start = dash_before(bytes, first),
            Some(first) => {
                node.flow = bytes.get(node.start) == Some(&b'[');
                if !node.flow && bytes.get(node.start) != Some(&b'-') {
                    node.start = dash_before(bytes, first);
                }
            }
        },
        Kind::Scalar => {}
    }
}
/// Parse one YAML document into nodes with byte offsets into `text`.
pub(crate) fn parse_tree(text: &str) -> Result<Node> {
    parse_documents(text, false)?.ok_or_else(|| invalid("empty YAML document"))
}
/// Reads the first document only; `None` when the stream has none.
pub fn parse_first_document(text: &str) -> Result<Option<Node>> {
    parse_documents(text, true)
}
fn parse_documents(text: &str, first_only: bool) -> Result<Option<Node>> {
    let bytes = text.as_bytes();
    // yaml-rust2 advances Marker.index once per character, not per byte.
    let mut offsets: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
    offsets.push(text.len());
    let mut parser = Parser::new_from_str(text);
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut count = 0;
    loop {
        let (event, mark) = parser.next_token().map_err(invalid)?;
        let offset = *offsets.get(mark.index()).unwrap_or(&text.len());
        let (mut node, container) = match event {
            Event::StreamEnd => break,
            Event::DocumentEnd if first_only => break,
            Event::Alias(_) => return Err(invalid("YAML aliases are unsupported")),
            Event::MappingEnd | Event::SequenceEnd => {
                let mut node = stack.pop().ok_or_else(|| invalid("invalid YAML nesting"))?;
                node.last = count - 1;
                finish(&mut node, bytes);
                match stack.last_mut() {
                    Some(parent) => parent.content.push(node),
                    None if root.is_none() => root = Some(node),
                    None => return Err(invalid("expected a single document in the stream")),
                }
                continue;
            }
            Event::MappingStart(..) | Event::SequenceStart(..) => {
                let (kind, default, anchor, tag) = match event {
                    Event::MappingStart(anchor, tag) => (Kind::Mapping, MAP, anchor, tag),
                    Event::SequenceStart(anchor, tag) => (Kind::Sequence, SEQ, anchor, tag),
                    _ => unreachable!(),
                };
                let mut node = Node::new(kind, default);
                node.anchored = anchor != 0;
                if let Some(tag) = tag.as_ref().map(short_tag).filter(|tag| tag != "!") {
                    node.tagged = true;
                    node.tag = tag;
                }
                (node, true)
            }
            Event::Scalar(value, style, anchor, tag) => {
                let style = match style {
                    TScalarStyle::Plain => Style::Plain,
                    TScalarStyle::SingleQuoted => Style::Single,
                    TScalarStyle::DoubleQuoted => Style::Double,
                    TScalarStyle::Literal => Style::Literal,
                    TScalarStyle::Folded => Style::Folded,
                };
                let default = if style != Style::Plain {
                    STR
                } else if value == "<<" {
                    MERGE
                } else {
                    resolve(&value)
                };
                let mut node = Node::scalar(&value, default, style);
                node.anchored = anchor != 0;
                node.empty = style == Style::Plain && value.is_empty();
                if let Some(tag) = tag.as_ref().map(short_tag).filter(|tag| tag != "!") {
                    node.tagged = true;
                    node.tag = tag;
                }
                (node, false)
            }
            _ => continue,
        };
        if stack.len() > 128 {
            return Err(invalid("Unity YAML nesting exceeds 128"));
        }
        node.index = count;
        node.last = count;
        node.start = offset;
        if matches!(node.style, Style::Literal | Style::Folded) {
            // yaml-rust2 marks block scalars at their content; here they start at the indicator.
            let indicator = if node.style == Style::Literal {
                '|'
            } else {
                '>'
            };
            if let Some(position) = text[..offset].rfind(indicator) {
                node.start = position;
            }
        }
        count += 1;
        if container {
            stack.push(node);
        } else {
            match stack.last_mut() {
                Some(parent) => parent.content.push(node),
                None if root.is_none() => root = Some(node),
                None => return Err(invalid("expected a single document in the stream")),
            }
        }
    }
    if !stack.is_empty() {
        return Err(invalid("incomplete YAML document"));
    }
    Ok(root)
}

// ---- decoding into values ----

/// Decodes a node into an untyped value, as its JSON form sees the result.
pub(crate) fn decode(node: &Node) -> Result<Value> {
    if node.anchored {
        return Err(unported("an anchor"));
    }
    match node.kind {
        Kind::Mapping => {
            let mut out = Map::new();
            for pair in node.content.as_chunks::<2>().0 {
                if pair[0].kind != Kind::Scalar || pair[0].tag == MERGE {
                    return Err(unported("a merge or collection key"));
                }
                if pair[0].tag != STR {
                    return Err(internal(
                        "json: unsupported type: mapping with non-string keys",
                    ));
                }
                out.insert(pair[0].value.clone(), decode(&pair[1])?);
            }
            Ok(Value::Object(out))
        }
        Kind::Sequence => Ok(Value::Array(
            node.content.iter().map(decode).collect::<Result<_>>()?,
        )),
        Kind::Scalar if node.tag == STR => Ok(Value::String(node.value.clone())),
        Kind::Scalar if node.tagged => Err(unported("an explicit tag")),
        Kind::Scalar => Ok(match resolve_value(&node.value) {
            Resolved::Null => Value::Null,
            Resolved::Bool(value) => Value::Bool(value),
            Resolved::Int(value) => Value::from(value),
            Resolved::Uint(value) => Value::from(value),
            Resolved::Float(value) => Value::Number(float_number(value)?),
            Resolved::Timestamp(value) => Value::String(value),
            Resolved::Str => Value::String(node.value.clone()),
        }),
    }
}

// ---- encoding ----

fn string_draft(text: &str) -> Node {
    let plain = resolve(text) == STR && !is_base60(text) && !is_old_bool(text);
    let style = if text.contains('\n') {
        Style::Literal
    } else if plain {
        Style::Plain
    } else {
        Style::Double
    };
    Node::scalar(text, "", style)
}
fn draft(value: &Value) -> Node {
    match value {
        Value::Null => Node::scalar("null", "", Style::Plain),
        Value::Bool(value) => Node::scalar(if *value { "true" } else { "false" }, "", Style::Plain),
        Value::Number(number) => Node::scalar(&number_text(number), "", Style::Plain),
        Value::String(text) => string_draft(text),
        Value::Array(items) => {
            let mut node = Node::new(Kind::Sequence, "");
            node.content = items.iter().map(draft).collect();
            node
        }
        Value::Object(map) => {
            let mut node = Node::new(Kind::Mapping, "");
            for key in sorted_keys(map) {
                node.content.push(string_draft(key));
                node.content.push(draft(&map[key]));
            }
            node
        }
    }
}
/// Encodes a value with the default indent, then reparses it into a node without positions.
pub(crate) fn encode_value(value: &Value) -> Result<Node> {
    let draft = draft(value);
    let mut node = parse_tree(&emit(&draft, 4)?)?;
    restore_values(&mut node, &draft);
    Ok(node)
}
/// The emitter's YAML 1.1 line separators read back unchanged; yaml-rust2
/// does not treat them as breaks, so scalar text is taken from the encoded value.
fn restore_values(node: &mut Node, draft: &Node) {
    if node.kind != draft.kind || node.content.len() != draft.content.len() {
        return;
    }
    if node.kind == Kind::Scalar && node.value != draft.value {
        node.value = draft.value.clone();
        if node.style == Style::Plain {
            node.tag = resolve(&node.value).into();
        }
    }
    for (child, source) in node.content.iter_mut().zip(&draft.content) {
        restore_values(child, source);
    }
}
fn normalize_ids(node: &mut Node) {
    if node.kind == Kind::Mapping {
        for pair in node.content.as_chunks_mut::<2>().0 {
            if pair[0].value == "fileID" && pair[1].kind == Kind::Scalar {
                pair[1].tag = INT.into();
                pair[1].tagged = false;
                pair[1].style = Style::Plain;
            }
        }
    }
    for child in &mut node.content {
        normalize_ids(child);
    }
}
fn flow_style(node: &mut Node) {
    if node.kind != Kind::Scalar {
        node.flow = true;
    }
    for child in &mut node.content {
        flow_style(child);
    }
}
/// A new block-style document node whose fileID scalars are plain integers.
pub(crate) fn document_node(value: &Value) -> Result<Node> {
    let mut node = encode_value(value)?;
    normalize_ids(&mut node);
    Ok(node)
}
/// Flow collections and plain integer fileIDs.
pub(crate) fn value_node(value: &Value) -> Result<Node> {
    let mut node = encode_value(value)?;
    flow_style(&mut node);
    normalize_ids(&mut node);
    Ok(node)
}
/// Encodes with two-space indentation, without the final newline.
pub(crate) fn encode_node(node: &Node) -> Result<String> {
    let text = emit(node, 2)?;
    Ok(text.strip_suffix('\n').map(String::from).unwrap_or(text))
}

// ---- emitter ----

struct Analysis {
    multiline: bool,
    flow_plain: bool,
    block_plain: bool,
    single_quoted: bool,
    block: bool,
}
fn width(byte: u8) -> usize {
    match byte {
        b if b & 0x80 == 0 => 1,
        b if b & 0xE0 == 0xC0 => 2,
        b if b & 0xF0 == 0xE0 => 3,
        _ => 4,
    }
}
fn at(value: &[u8], index: usize) -> u8 {
    value.get(index).copied().unwrap_or(0)
}
fn is_space(value: &[u8], index: usize) -> bool {
    at(value, index) == b' '
}
fn is_blank(value: &[u8], index: usize) -> bool {
    matches!(at(value, index), b' ' | b'\t')
}
fn is_break(value: &[u8], index: usize) -> bool {
    let (a, b, c) = (at(value, index), at(value, index + 1), at(value, index + 2));
    a == b'\r'
        || a == b'\n'
        || (a == 0xC2 && b == 0x85)
        || (a == 0xE2 && b == 0x80 && (c == 0xA8 || c == 0xA9))
}
fn is_blankz(value: &[u8], index: usize) -> bool {
    is_blank(value, index) || is_break(value, index) || at(value, index) == 0
}
fn is_printable(value: &[u8], index: usize) -> bool {
    let (a, b, c) = (at(value, index), at(value, index + 1), at(value, index + 2));
    a == 0x0A
        || (0x20..=0x7E).contains(&a)
        || (a == 0xC2 && b >= 0xA0)
        || (a > 0xC2 && a < 0xED)
        || (a == 0xED && b < 0xA0)
        || a == 0xEE
        || (a == 0xEF && !(b == 0xBB && c == 0xBF) && !(b == 0xBF && (c == 0xBE || c == 0xBF)))
}
fn is_bom(value: &[u8]) -> bool {
    value.starts_with(&[0xEF, 0xBB, 0xBF])
}
fn analyze(value: &[u8]) -> Analysis {
    if value.is_empty() {
        return Analysis {
            multiline: false,
            flow_plain: false,
            block_plain: true,
            single_quoted: true,
            block: false,
        };
    }
    let (mut block_indicators, mut flow_indicators) = (false, false);
    let (mut line_breaks, mut special, mut tabs) = (false, false, false);
    let (mut leading_space, mut leading_break) = (false, false);
    let (mut trailing_space, mut trailing_break) = (false, false);
    let (mut break_space, mut space_break) = (false, false);
    let (mut previous_space, mut previous_break) = (false, false);
    if value.starts_with(b"---") || value.starts_with(b"...") {
        block_indicators = true;
        flow_indicators = true;
    }
    let mut preceded = true;
    let mut i = 0;
    while i < value.len() {
        let w = width(value[i]);
        let followed = i + w >= value.len() || is_blank(value, i + w);
        if i == 0 {
            match value[i] {
                b'#' | b',' | b'[' | b']' | b'{' | b'}' | b'&' | b'*' | b'!' | b'|' | b'>'
                | b'\'' | b'"' | b'%' | b'@' | b'`' => {
                    flow_indicators = true;
                    block_indicators = true;
                }
                b'?' | b':' => {
                    flow_indicators = true;
                    block_indicators |= followed;
                }
                b'-' if followed => {
                    flow_indicators = true;
                    block_indicators = true;
                }
                _ => {}
            }
        } else {
            match value[i] {
                b',' | b'?' | b'[' | b']' | b'{' | b'}' => flow_indicators = true,
                b':' => {
                    flow_indicators = true;
                    block_indicators |= followed;
                }
                b'#' if preceded => {
                    flow_indicators = true;
                    block_indicators = true;
                }
                _ => {}
            }
        }
        if value[i] == b'\t' {
            tabs = true;
        } else if !is_printable(value, i) {
            special = true;
        }
        if is_space(value, i) {
            leading_space |= i == 0;
            trailing_space |= i + w == value.len();
            break_space |= previous_break;
            previous_space = true;
            previous_break = false;
        } else if is_break(value, i) {
            line_breaks = true;
            leading_break |= i == 0;
            trailing_break |= i + w == value.len();
            space_break |= previous_space;
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }
        preceded = is_blankz(value, i);
        i += w;
    }
    let mut result = Analysis {
        multiline: line_breaks,
        flow_plain: true,
        block_plain: true,
        single_quoted: true,
        block: true,
    };
    if leading_space || leading_break || trailing_space || trailing_break {
        result.flow_plain = false;
        result.block_plain = false;
    }
    if trailing_space {
        result.block = false;
    }
    if break_space {
        result.flow_plain = false;
        result.block_plain = false;
        result.single_quoted = false;
    }
    if space_break || tabs || special {
        result.flow_plain = false;
        result.block_plain = false;
        result.single_quoted = false;
    }
    if space_break || special {
        result.block = false;
    }
    if line_breaks {
        result.flow_plain = false;
        result.block_plain = false;
    }
    if flow_indicators {
        result.flow_plain = false;
    }
    if block_indicators {
        result.block_plain = false;
    }
    result
}
/// The style requested from the emitter once implicit tags are dropped.
fn requested_style(node: &Node) -> Result<Style> {
    if node.anchored {
        return Err(unported("an anchor"));
    }
    if node.tagged {
        return Err(unported("an explicit tag"));
    }
    let mut force = false;
    if !node.tag.is_empty() && !(node.tag == STR && node.style != Style::Plain) {
        let resolved = resolve(&node.value);
        if resolved != node.tag {
            if node.tag != STR {
                return Err(unported("an explicit tag"));
            }
            force = true;
        }
    }
    Ok(match node.style {
        Style::Plain if node.value.contains('\n') => Style::Literal,
        Style::Plain if force => Style::Double,
        style => style,
    })
}
fn collection_style(node: &Node) -> Result<()> {
    if node.anchored {
        return Err(unported("an anchor"));
    }
    if node.tagged {
        return Err(unported("an explicit tag"));
    }
    Ok(())
}
struct Emitter {
    out: Vec<u8>,
    column: usize,
    indent: i32,
    indents: Vec<i32>,
    whitespace: bool,
    indention: bool,
    flow_level: usize,
    best_indent: i32,
}
impl Emitter {
    fn put(&mut self, byte: u8) {
        self.out.push(byte);
        self.column += 1;
    }
    fn put_break(&mut self) {
        self.out.push(b'\n');
        self.column = 0;
        self.indention = true;
    }
    fn write(&mut self, value: &[u8], index: &mut usize) {
        let w = width(value[*index]);
        self.out.extend_from_slice(&value[*index..*index + w]);
        self.column += 1;
        *index += w;
    }
    fn write_break(&mut self, value: &[u8], index: &mut usize) {
        if value[*index] == b'\n' {
            self.put_break();
            *index += 1;
        } else {
            self.write(value, index);
            self.column = 0;
            self.indention = true;
        }
    }
    fn write_indent(&mut self) {
        let indent = self.indent.max(0) as usize;
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.put_break();
        }
        while self.column < indent {
            self.put(b' ');
        }
        self.whitespace = true;
    }
    fn indicator(&mut self, text: &[u8], need_whitespace: bool, whitespace: bool, indention: bool) {
        if need_whitespace && !self.whitespace {
            self.put(b' ');
        }
        let mut index = 0;
        while index < text.len() {
            self.write(text, &mut index);
        }
        self.whitespace = whitespace;
        self.indention = self.indention && indention;
    }
    fn increase_indent(&mut self, flow: bool, sequence_item: bool) {
        self.indents.push(self.indent);
        if self.indent < 0 {
            self.indent = if flow { self.best_indent } else { 0 };
        } else if sequence_item {
            // The first indent inside a block sequence only skips its "- ".
            self.indent += 2;
        } else {
            self.indent = self.best_indent * ((self.indent + self.best_indent) / self.best_indent);
        }
    }
    fn decrease_indent(&mut self) {
        self.indent = self.indents.pop().unwrap();
    }
    fn simple_key(node: &Node) -> Result<bool> {
        Ok(match node.kind {
            Kind::Scalar => {
                requested_style(node)?;
                !analyze(node.value.as_bytes()).multiline && node.value.len() <= 128
            }
            _ => node.content.is_empty(),
        })
    }
    /// `sequence_item` mirrors the emitter finding EMIT_BLOCK_SEQUENCE_ITEM on its state stack.
    fn node(&mut self, node: &Node, sequence_item: bool, simple_key: bool) -> Result<()> {
        match node.kind {
            Kind::Scalar => self.scalar(node, sequence_item, simple_key),
            Kind::Sequence => {
                collection_style(node)?;
                if self.flow_level > 0 || node.flow || node.content.is_empty() {
                    self.indicator(b"[", true, true, false);
                    self.increase_indent(true, sequence_item);
                    self.flow_level += 1;
                    for (index, item) in node.content.iter().enumerate() {
                        if index > 0 {
                            self.indicator(b",", false, false, false);
                        }
                        if self.column == 0 {
                            self.write_indent();
                        }
                        self.node(item, false, false)?;
                    }
                    self.flow_level -= 1;
                    self.decrease_indent();
                    if self.column == 0 {
                        self.write_indent();
                    }
                    self.indicator(b"]", false, false, false);
                } else {
                    self.increase_indent(false, sequence_item);
                    for item in &node.content {
                        self.write_indent();
                        self.indicator(b"-", true, false, true);
                        self.node(item, true, false)?;
                    }
                    self.decrease_indent();
                }
                Ok(())
            }
            Kind::Mapping => {
                collection_style(node)?;
                let flow = self.flow_level > 0 || node.flow || node.content.is_empty();
                if flow {
                    self.indicator(b"{", true, true, false);
                }
                self.increase_indent(flow, sequence_item);
                if flow {
                    self.flow_level += 1;
                }
                for (index, pair) in node.content.as_chunks::<2>().0.iter().enumerate() {
                    if flow {
                        if index > 0 {
                            self.indicator(b",", false, false, false);
                        }
                        if self.column == 0 {
                            self.write_indent();
                        }
                    } else {
                        self.write_indent();
                    }
                    if Self::simple_key(&pair[0])? {
                        self.node(&pair[0], false, true)?;
                        self.indicator(b":", false, false, false);
                    } else {
                        self.indicator(b"?", true, false, !flow);
                        self.node(&pair[0], false, false)?;
                        if !flow {
                            self.write_indent();
                        }
                        self.indicator(b":", true, false, !flow);
                    }
                    self.node(&pair[1], false, false)?;
                }
                if flow {
                    self.flow_level -= 1;
                }
                self.decrease_indent();
                if flow {
                    self.indicator(b"}", false, false, false);
                }
                Ok(())
            }
        }
    }
    fn scalar(&mut self, node: &Node, sequence_item: bool, simple_key: bool) -> Result<()> {
        let value = node.value.as_bytes();
        let analysis = analyze(value);
        let mut style = requested_style(node)?;
        if simple_key && analysis.multiline {
            style = Style::Double;
        }
        if style == Style::Plain
            && ((self.flow_level > 0 && !analysis.flow_plain)
                || (self.flow_level == 0 && !analysis.block_plain)
                || (value.is_empty() && (self.flow_level > 0 || simple_key)))
        {
            style = Style::Single;
        }
        if style == Style::Single && !analysis.single_quoted {
            style = Style::Double;
        }
        if matches!(style, Style::Literal | Style::Folded)
            && (!analysis.block || self.flow_level > 0 || simple_key)
        {
            style = Style::Double;
        }
        self.increase_indent(true, sequence_item);
        match style {
            Style::Plain => self.plain(value),
            Style::Single => self.single_quoted(value),
            Style::Double => self.double_quoted(value),
            Style::Literal => self.literal(value),
            Style::Folded => self.folded(value),
        }
        self.decrease_indent();
        Ok(())
    }
    // The emitter's best_width is unlimited, so no scalar is ever folded for width.
    fn plain(&mut self, value: &[u8]) {
        if !value.is_empty() && !self.whitespace {
            self.put(b' ');
        }
        let mut breaks = false;
        let mut index = 0;
        while index < value.len() {
            if is_space(value, index) {
                self.write(value, &mut index);
            } else if is_break(value, index) {
                if !breaks && value[index] == b'\n' {
                    self.put_break();
                }
                self.write_break(value, &mut index);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.write(value, &mut index);
                self.indention = false;
                breaks = false;
            }
        }
        if !value.is_empty() {
            self.whitespace = false;
        }
        self.indention = false;
    }
    fn single_quoted(&mut self, value: &[u8]) {
        self.indicator(b"'", true, false, false);
        let mut breaks = false;
        let mut index = 0;
        while index < value.len() {
            if is_space(value, index) {
                self.write(value, &mut index);
            } else if is_break(value, index) {
                if !breaks && value[index] == b'\n' {
                    self.put_break();
                }
                self.write_break(value, &mut index);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                if value[index] == b'\'' {
                    self.put(b'\'');
                }
                self.write(value, &mut index);
                self.indention = false;
                breaks = false;
            }
        }
        self.indicator(b"'", false, false, false);
        self.whitespace = false;
        self.indention = false;
    }
    fn double_quoted(&mut self, value: &[u8]) {
        self.indicator(b"\"", true, false, false);
        let mut index = 0;
        while index < value.len() {
            if !is_printable(value, index)
                || is_bom(value)
                || is_break(value, index)
                || value[index] == b'"'
                || value[index] == b'\\'
            {
                let w = width(value[index]);
                let code = std::str::from_utf8(&value[index..index + w])
                    .ok()
                    .and_then(|text| text.chars().next())
                    .map_or(0, |c| c as u32);
                index += w;
                self.put(b'\\');
                let short = match code {
                    0x00 => Some(b'0'),
                    0x07 => Some(b'a'),
                    0x08 => Some(b'b'),
                    0x09 => Some(b't'),
                    0x0A => Some(b'n'),
                    0x0B => Some(b'v'),
                    0x0C => Some(b'f'),
                    0x0D => Some(b'r'),
                    0x1B => Some(b'e'),
                    0x22 => Some(b'"'),
                    0x5C => Some(b'\\'),
                    0x85 => Some(b'N'),
                    0xA0 => Some(b'_'),
                    0x2028 => Some(b'L'),
                    0x2029 => Some(b'P'),
                    _ => None,
                };
                match short {
                    Some(byte) => self.put(byte),
                    None => {
                        let text = if code <= 0xFF {
                            format!("x{code:02X}")
                        } else if code <= 0xFFFF {
                            format!("u{code:04X}")
                        } else {
                            format!("U{code:08X}")
                        };
                        for byte in text.bytes() {
                            self.put(byte);
                        }
                    }
                }
            } else {
                self.write(value, &mut index);
            }
        }
        self.indicator(b"\"", false, false, false);
        self.whitespace = false;
        self.indention = false;
    }
    fn block_hints(&mut self, value: &[u8]) {
        if is_space(value, 0) || is_break(value, 0) {
            let hint = [b'0' + self.best_indent as u8];
            self.indicator(&hint, false, false, false);
        }
        let mut chomp = None;
        if value.is_empty() {
            chomp = Some(b'-');
        } else {
            let mut index = value.len() - 1;
            while value[index] & 0xC0 == 0x80 {
                index -= 1;
            }
            if !is_break(value, index) {
                chomp = Some(b'-');
            } else if index == 0 {
                chomp = Some(b'+');
            } else {
                index -= 1;
                while value[index] & 0xC0 == 0x80 {
                    index -= 1;
                }
                if is_break(value, index) {
                    chomp = Some(b'+');
                }
            }
        }
        if let Some(chomp) = chomp {
            self.indicator(&[chomp], false, false, false);
        }
    }
    fn literal(&mut self, value: &[u8]) {
        self.indicator(b"|", true, false, false);
        self.block_hints(value);
        self.whitespace = true;
        let mut breaks = true;
        let mut index = 0;
        while index < value.len() {
            if is_break(value, index) {
                self.write_break(value, &mut index);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.write(value, &mut index);
                self.indention = false;
                breaks = false;
            }
        }
    }
    fn folded(&mut self, value: &[u8]) {
        self.indicator(b">", true, false, false);
        self.block_hints(value);
        self.whitespace = true;
        let (mut breaks, mut leading_spaces) = (true, true);
        let mut index = 0;
        while index < value.len() {
            if is_break(value, index) {
                if !breaks && !leading_spaces && value[index] == b'\n' {
                    let mut k = 0;
                    while is_break(value, k) {
                        k += width(value[k]);
                    }
                    if !is_blankz(value, k) {
                        self.put_break();
                    }
                }
                self.write_break(value, &mut index);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                    leading_spaces = is_blank(value, index);
                }
                self.write(value, &mut index);
                self.indention = false;
                breaks = false;
            }
        }
    }
}
fn emit(node: &Node, best_indent: i32) -> Result<String> {
    let mut emitter = Emitter {
        out: Vec::new(),
        column: 0,
        indent: -1,
        indents: Vec::new(),
        whitespace: true,
        indention: true,
        flow_level: 0,
        best_indent,
    };
    emitter.node(node, false, false)?;
    emitter.write_indent();
    String::from_utf8(emitter.out).map_err(|error| internal(error.to_string()))
}
