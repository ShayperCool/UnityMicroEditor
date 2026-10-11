//! A dynamic value that keeps distinctions a plain JSON value loses: YAML integers and
//! floats, JSON number literals and typed integers format and compare differently.
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{borrow::Cow, cell::Cell, collections::BTreeMap, fmt::Write as _, io::Read, sync::Arc};

pub type Map = BTreeMap<String, Value>;

#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    String(String),
    /// A 64-bit signed integer.
    Int(i64),
    /// A 32-bit signed integer; `number` does not convert it.
    Int32(i32),
    Uint(u64),
    Float(f64),
    /// A JSON number literal: published verbatim.
    Number(String),
    Array(Vec<Value>),
    Object(Map),
    /// Already encoded JSON of a typed struct or list.
    Raw(Arc<str>),
}

pub static NULL: Value = Value::Null;
static EMPTY: Map = Map::new();

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::String(value.into())
    }
}
impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::String(value)
    }
}
impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}
impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Value::Int(value)
    }
}
impl From<Map> for Value {
    fn from(value: Map) -> Self {
        Value::Object(value)
    }
}
impl From<Vec<Value>> for Value {
    fn from(value: Vec<Value>) -> Self {
        Value::Array(value)
    }
}

/// Builds an object from literal pairs.
pub fn map<const N: usize>(pairs: [(&str, Value); N]) -> Map {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// The value at `key`, `Null` when the key is missing.
pub fn get<'a>(map: &'a Map, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&NULL)
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }
    pub fn as_object(&self) -> Option<&Map> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }
    pub fn as_object_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
    /// The member `key` of this value as an object, `Null` when absent.
    pub fn field(&self, key: &str) -> &Value {
        get(object(self), key)
    }
    /// True only for the boolean `true`.
    pub fn is_true(&self) -> bool {
        matches!(self, Value::Bool(true))
    }
    pub fn is_false(&self) -> bool {
        matches!(self, Value::Bool(false))
    }
    /// True for map values carrying a `fileID` key.
    pub fn is_reference(&self) -> bool {
        matches!(self, Value::Object(map) if map.contains_key("fileID"))
    }
    pub fn to_json(&self) -> Result<String> {
        let mut out = String::new();
        self.encode(&mut out)?;
        Ok(out)
    }
    /// JSON output: sorted keys, HTML escaping, shortest round-trip float layout.
    pub fn encode(&self, out: &mut String) -> Result<()> {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::String(text) => encode_string(text, out),
            Value::Int(n) => write!(out, "{n}")?,
            Value::Int32(n) => write!(out, "{n}")?,
            Value::Uint(n) => write!(out, "{n}")?,
            Value::Float(f) => out.push_str(&json_float(*f)?),
            Value::Number(text) => {
                if text.is_empty() {
                    out.push('0');
                } else if valid_number(text) {
                    out.push_str(text);
                } else {
                    bail!("json: invalid number literal {text:?}");
                }
            }
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.encode(out)?;
                }
                out.push(']');
            }
            Value::Object(map) => encode_map(map, out)?,
            Value::Raw(text) => out.push_str(text),
        }
        Ok(())
    }
    /// Converts a parsed Unity YAML value; `crate::yaml` writes floats with a
    /// fraction or exponent and integers without either.
    pub fn from_yaml(value: &serde_json::Value) -> Value {
        match value {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::String(text) => Value::String(text.clone()),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if let Some(u) = n.as_u64() {
                    Value::Uint(u)
                } else {
                    Value::Float(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            serde_json::Value::Array(items) => {
                Value::Array(items.iter().map(Value::from_yaml).collect())
            }
            serde_json::Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), Value::from_yaml(v)))
                    .collect(),
            ),
        }
    }
}

pub fn encode_map(map: &Map, out: &mut String) -> Result<()> {
    out.push('{');
    for (index, (key, value)) in map.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        encode_string(key, out);
        out.push(':');
        value.encode(out)?;
    }
    out.push('}');
    Ok(())
}

pub fn encode_string(text: &str, out: &mut String) {
    out.push('"');
    let mut start = 0;
    for (index, c) in text.char_indices() {
        let escape: Option<Cow<'static, str>> = match c {
            '"' => Some("\\\"".into()),
            '\\' => Some("\\\\".into()),
            '\u{8}' => Some("\\b".into()),
            '\u{c}' => Some("\\f".into()),
            '\n' => Some("\\n".into()),
            '\r' => Some("\\r".into()),
            '\t' => Some("\\t".into()),
            '<' => Some("\\u003c".into()),
            '>' => Some("\\u003e".into()),
            '&' => Some("\\u0026".into()),
            '\u{2028}' => Some("\\u2028".into()),
            '\u{2029}' => Some("\\u2029".into()),
            c if (c as u32) < 0x20 => Some(format!("\\u{:04x}", c as u32).into()),
            _ => None,
        };
        if let Some(escape) = escape {
            out.push_str(&text[start..index]);
            out.push_str(&escape);
            start = index + c.len_utf8();
        }
    }
    out.push_str(&text[start..]);
    out.push('"');
}

/// JSON of a typed value: serde field order plus HTML escaping of `&`, `<`, `>`, U+2028 and U+2029.
pub fn encode_json<T: Serialize>(value: &T) -> Result<String> {
    let text = serde_json::to_string(value)?;
    if !text.contains(['&', '<', '>', '\u{2028}', '\u{2029}']) {
        return Ok(text);
    }
    Ok(text
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

pub fn raw<T: Serialize>(value: &T) -> Result<Value> {
    Ok(Value::Raw(encode_json(value)?.into()))
}

fn valid_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    if bytes.first() == Some(&b'-') {
        i += 1;
    }
    match bytes.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        _ => return false,
    }
    if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 2;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !bytes.get(i).is_some_and(u8::is_ascii_digit) {
            return false;
        }
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    i == bytes.len()
}

/// Shortest round-trip digits and decimal exponent of a finite nonzero float.
fn digits(f: f64) -> (String, i32) {
    let text = format!("{:e}", f.abs());
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    (mantissa.replace('.', ""), exponent.parse().unwrap_or(0))
}
fn fixed(digits: &str, exponent: i32) -> String {
    if exponent < 0 {
        return format!("0.{}{digits}", "0".repeat((-exponent - 1) as usize));
    }
    let whole = exponent as usize + 1;
    if digits.len() <= whole {
        format!("{digits}{}", "0".repeat(whole - digits.len()))
    } else {
        format!("{}.{}", &digits[..whole], &digits[whole..])
    }
}
fn scientific(digits: &str, exponent: i32) -> String {
    let mut text = digits[..1].to_string();
    if digits.len() > 1 {
        text.push('.');
        text.push_str(&digits[1..]);
    }
    let sign = if exponent < 0 { '-' } else { '+' };
    let _ = write!(text, "e{sign}{:02}", exponent.abs());
    text
}

/// Shortest round-trip text of an f64, in exponent form below 1e-4 or from 1e6 up;
/// also its default display text.
pub fn float_layout(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf" } else { "-Inf" }.into();
    }
    let sign = if f.is_sign_negative() { "-" } else { "" };
    if f == 0.0 {
        return format!("{sign}0");
    }
    let (digits, exponent) = digits(f);
    // The shortest form switches to an exponent outside -4..6.
    if !(-4..6).contains(&exponent) {
        return format!("{sign}{}", scientific(&digits, exponent));
    }
    format!("{sign}{}", fixed(&digits, exponent))
}

/// JSON text of an f64: fixed notation inside 1e-6..1e21, exponent form outside.
pub fn json_float(f: f64) -> Result<String> {
    if !f.is_finite() {
        bail!("json: unsupported value: {}", float_layout(f));
    }
    let sign = if f.is_sign_negative() { "-" } else { "" };
    if f == 0.0 {
        return Ok(format!("{sign}0"));
    }
    let (digits, exponent) = digits(f);
    let magnitude = f.abs();
    if !(1e-6..1e21).contains(&magnitude) {
        let mut text = scientific(&digits, exponent);
        // e-09 is trimmed to e-9; positive exponents keep their form.
        let bytes = text.as_bytes();
        let n = bytes.len();
        if n >= 4 && bytes[n - 4] == b'e' && bytes[n - 3] == b'-' && bytes[n - 2] == b'0' {
            text = format!("{}{}", &text[..n - 2], &text[n - 1..]);
        }
        return Ok(format!("{sign}{text}"));
    }
    Ok(format!("{sign}{}", fixed(&digits, exponent)))
}

/// Default display text of a value; null prints as `<nil>`.
pub fn sprint(value: &Value) -> String {
    match value {
        Value::Null => "<nil>".into(),
        Value::Bool(b) => b.to_string(),
        Value::String(text) => text.clone(),
        Value::Int(n) => n.to_string(),
        Value::Int32(n) => n.to_string(),
        Value::Uint(n) => n.to_string(),
        Value::Float(f) => float_layout(*f),
        Value::Number(text) => text.clone(),
        Value::Array(items) => {
            format!(
                "[{}]",
                items.iter().map(sprint).collect::<Vec<_>>().join(" ")
            )
        }
        Value::Object(map) => format!(
            "map[{}]",
            map.iter()
                .map(|(k, v)| format!("{k}:{}", sprint(v)))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Value::Raw(text) => text.to_string(),
    }
}

/// Text of a value: null is empty, strings and JSON numbers are verbatim.
pub fn str(value: &Value) -> Cow<'_, str> {
    match value {
        Value::Null => Cow::Borrowed(""),
        Value::String(text) | Value::Number(text) => Cow::Borrowed(text),
        other => Cow::Owned(sprint(other)),
    }
}

/// Integer of a value: only Int, Float and Number convert.
pub fn number(value: &Value) -> i64 {
    match value {
        Value::Int(n) => *n,
        Value::Int32(n) => i64::from(*n),
        // NaN and out-of-range floats become the minimum i64, as the recorded cases expect.
        Value::Float(f) => {
            const LIMIT: f64 = 9_223_372_036_854_775_808.0;
            if f.is_nan() || *f >= LIMIT || *f < -LIMIT {
                i64::MIN
            } else {
                *f as i64
            }
        }
        // A parse error is ignored: overflow saturates.
        Value::Number(text) => super::strconv::atoi_lossy(text),
        _ => 0,
    }
}

pub fn object(value: &Value) -> &Map {
    value.as_object().unwrap_or(&EMPTY)
}

pub fn array(value: &Value) -> &[Value] {
    match value {
        Value::Array(items) => items,
        _ => &[],
    }
}

/// The reference's local ID, `"0"` when absent.
pub fn file_id(value: &Value) -> Cow<'_, str> {
    map_file_id(object(value))
}
pub fn map_file_id(map: &Map) -> Cow<'_, str> {
    let id = str(get(map, "fileID"));
    if id.is_empty() {
        Cow::Borrowed("0")
    } else {
        id
    }
}
/// The text of the value at `key`.
pub fn text<'a>(map: &'a Map, key: &str) -> Cow<'a, str> {
    str(get(map, key))
}

/// An exact decimal: sign, significant digits without leading/trailing zeros, exponent.
#[derive(PartialEq, Eq, Debug)]
pub struct Decimal {
    negative: bool,
    digits: String,
    exponent: i64,
}

/// Parses the exact decimal forms reachable from formatted numeric values.
pub fn decimal(text: &str) -> Option<Decimal> {
    let (negative, rest) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(index) => (&rest[..index], rest[index + 1..].parse::<i64>().ok()?),
        None => (rest, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let all = format!("{whole}{fraction}");
    // A zero mantissa is accepted before the exponent range is checked.
    if all.bytes().all(|b| b == b'0') {
        return Some(Decimal {
            negative: false,
            digits: String::new(),
            exponent: 0,
        });
    }
    let mut exponent = exponent.checked_sub(fraction.len() as i64)?;
    if !(-1_000_000..=1_000_000).contains(&exponent) {
        return None;
    }
    let trimmed = all.trim_start_matches('0');
    let digits = trimmed.trim_end_matches('0');
    exponent += (trimmed.len() - digits.len()) as i64;
    if digits.is_empty() {
        return Some(Decimal {
            negative: false,
            digits: String::new(),
            exponent: 0,
        });
    }
    Some(Decimal {
        negative,
        digits: digits.into(),
        exponent,
    })
}
impl Decimal {
    /// The value when it is a whole number that fits in an i64.
    pub fn int64(&self) -> Option<i64> {
        if self.digits.is_empty() {
            return Some(0);
        }
        if self.exponent < 0 || self.digits.len() as i64 + self.exponent > 19 {
            return None;
        }
        let magnitude: i128 = format!("{}{}", self.digits, "0".repeat(self.exponent as usize))
            .parse()
            .ok()?;
        i64::try_from(if self.negative { -magnitude } else { magnitude }).ok()
    }
}

thread_local! {
    static USE_NUMBER: Cell<bool> = const { Cell::new(false) };
}
struct NumberMode(bool);
impl NumberMode {
    fn set(value: bool) -> Self {
        Self(USE_NUMBER.with(|flag| flag.replace(value)))
    }
}
impl Drop for NumberMode {
    fn drop(&mut self) {
        USE_NUMBER.with(|flag| flag.set(self.0));
    }
}

/// Typed fields decode numbers as f64; `decode_use_number` keeps number
/// literals verbatim for the duration of one decode.
impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // serde_json rewrites number literals (`-0`, `1E5`); keep the source text.
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        parse_json(raw.get().as_bytes(), USE_NUMBER.with(Cell::get))
            .map(|(value, _)| value)
            .map_err(serde::de::Error::custom)
    }
}
impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error, SerializeMap, SerializeSeq, SerializeStruct};
        // serde_json writes this struct's field verbatim (arbitrary_precision),
        // which keeps `-0`, exponents and number literals exactly as written.
        const VERBATIM: &str = "$serde_json::private::Number";
        let verbatim = |serializer: S, text: &str| {
            let mut state = serializer.serialize_struct(VERBATIM, 1)?;
            state.serialize_field(VERBATIM, text)?;
            state.end()
        };
        match self {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::String(text) => serializer.serialize_str(text),
            Value::Int(n) => serializer.serialize_i64(*n),
            Value::Int32(n) => serializer.serialize_i32(*n),
            Value::Uint(n) => serializer.serialize_u64(*n),
            Value::Float(_) | Value::Number(_) | Value::Raw(_) => {
                verbatim(serializer, &self.to_json().map_err(Error::custom)?)
            }
            Value::Array(items) => {
                let mut state = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    state.serialize_element(item)?;
                }
                state.end()
            }
            Value::Object(map) => {
                let mut state = serializer.serialize_map(Some(map.len()))?;
                for (key, item) in map {
                    state.serialize_entry(key, item)?;
                }
                state.end()
            }
        }
    }
}

/// Decodes one JSON document; numbers become f64.
pub fn decode<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T> {
    let _mode = NumberMode::set(false);
    Ok(serde_json::from_str(text)?)
}
/// A decoder that keeps number literals verbatim, reading one value and ignoring what follows.
pub fn decode_use_number<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T> {
    let _mode = NumberMode::set(true);
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<T>();
    stream
        .next()
        .ok_or_else(|| anyhow!("EOF"))?
        .map_err(Into::into)
}
pub fn parse_use_number(text: &str) -> Result<Value> {
    Ok(parse_json(text.as_bytes(), true)?.0)
}

/// Reads a JSON file: at most 256 MiB, number literals kept verbatim, no trailing content.
pub fn read_json<T: for<'de> Deserialize<'de>>(path: &str) -> Result<T> {
    let mut data = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| super::lexpath::os_error("open", path, e))?
        .take(256 << 20)
        .read_to_end(&mut data)?;
    let _mode = NumberMode::set(true);
    let mut stream = serde_json::Deserializer::from_slice(&data).into_iter::<T>();
    let value = stream.next().ok_or_else(|| anyhow!("EOF"))??;
    let rest = &data[stream.byte_offset()..];
    if !rest
        .iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
    {
        bail!("trailing JSON content: {path}");
    }
    Ok(value)
}

pub fn compact_json(value: &Value) -> Result<String> {
    value.to_json()
}
pub fn compact_map(map: &Map) -> Result<String> {
    let mut out = String::new();
    encode_map(map, &mut out)?;
    Ok(out)
}

enum Frame {
    Array(Vec<Value>),
    Object(Map, Option<String>),
}
struct JsonParser<'a> {
    data: &'a [u8],
    at: usize,
    use_number: bool,
}
impl JsonParser<'_> {
    fn fail<T>(&self, what: &str) -> Result<T> {
        if self.at >= self.data.len() {
            bail!("unexpected end of JSON input");
        }
        bail!(
            "invalid character {:?} {what}",
            char::from(self.data[self.at])
        )
    }
    fn space(&mut self) {
        while matches!(self.data.get(self.at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.at += 1;
        }
    }
    fn literal(&mut self, word: &str, value: Value) -> Result<Value> {
        if self.data[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            return Ok(value);
        }
        self.fail("in literal")
    }
    fn number(&mut self) -> Result<Value> {
        let start = self.at;
        let digits = |p: &mut Self| {
            let from = p.at;
            while p.data.get(p.at).is_some_and(u8::is_ascii_digit) {
                p.at += 1;
            }
            p.at > from
        };
        if self.data.get(self.at) == Some(&b'-') {
            self.at += 1;
        }
        match self.data.get(self.at) {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return self.fail("in numeric literal"),
        }
        if self.data.get(self.at) == Some(&b'.') {
            self.at += 1;
            if !digits(self) {
                return self.fail("after decimal point in numeric literal");
            }
        }
        if matches!(self.data.get(self.at), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.data.get(self.at), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !digits(self) {
                return self.fail("in exponent of numeric literal");
            }
        }
        let text = std::str::from_utf8(&self.data[start..self.at])?;
        if self.use_number {
            return Ok(Value::Number(text.into()));
        }
        match text.parse::<f64>() {
            Ok(parsed) if parsed.is_finite() => Ok(Value::Float(parsed)),
            _ => bail!("json: cannot unmarshal number {text} into value of type float64"),
        }
    }
    fn hex4(&mut self) -> Result<u32> {
        let digits = self
            .data
            .get(self.at..self.at + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()));
        let Some(digits) = digits else {
            return self.fail("in \\u hexadecimal character escape");
        };
        self.at += 4;
        Ok(u32::from_str_radix(digits, 16)?)
    }
    fn string(&mut self) -> Result<String> {
        self.at += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let start = self.at;
            while self
                .data
                .get(self.at)
                .is_some_and(|b| *b != b'"' && *b != b'\\' && *b >= 0x20)
            {
                self.at += 1;
            }
            out.extend_from_slice(&self.data[start..self.at]);
            match self.data.get(self.at) {
                Some(b'"') => {
                    self.at += 1;
                    break;
                }
                Some(b'\\') => {
                    self.at += 1;
                    let escape = self.data.get(self.at).copied();
                    self.at += 1;
                    let c = match escape {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => {
                            let first = self.hex4()?;
                            let mut c = char::from_u32(first).unwrap_or('\u{fffd}');
                            if (0xd800..0xdc00).contains(&first)
                                && self.data[self.at..].starts_with(b"\\u")
                            {
                                let rewind = self.at;
                                self.at += 2;
                                let second = self.hex4()?;
                                if (0xdc00..0xe000).contains(&second) {
                                    let joined =
                                        0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                                    c = char::from_u32(joined).unwrap_or('\u{fffd}');
                                } else {
                                    self.at = rewind;
                                }
                            }
                            c
                        }
                        _ => {
                            self.at -= 1;
                            return self.fail("in string escape code");
                        }
                    };
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                _ => return self.fail("in string literal"),
            }
        }
        // Invalid UTF-8 is replaced with U+FFFD while unquoting.
        Ok(match String::from_utf8(out) {
            Ok(text) => text,
            Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
        })
    }
    fn value(&mut self) -> Result<Value> {
        let mut stack: Vec<Frame> = Vec::new();
        loop {
            self.space();
            let mut value = match self.data.get(self.at) {
                Some(b'{') => {
                    self.at += 1;
                    self.space();
                    if self.data.get(self.at) == Some(&b'}') {
                        self.at += 1;
                        Value::Object(Map::new())
                    } else {
                        if self.data.get(self.at) != Some(&b'"') {
                            return self.fail("looking for beginning of object key string");
                        }
                        let key = self.string()?;
                        self.space();
                        if self.data.get(self.at) != Some(&b':') {
                            return self.fail("after object key");
                        }
                        self.at += 1;
                        if stack.len() >= 10000 {
                            bail!("exceeded max depth");
                        }
                        stack.push(Frame::Object(Map::new(), Some(key)));
                        continue;
                    }
                }
                Some(b'[') => {
                    self.at += 1;
                    self.space();
                    if self.data.get(self.at) == Some(&b']') {
                        self.at += 1;
                        Value::Array(Vec::new())
                    } else {
                        if stack.len() >= 10000 {
                            bail!("exceeded max depth");
                        }
                        stack.push(Frame::Array(Vec::new()));
                        continue;
                    }
                }
                Some(b'"') => Value::String(self.string()?),
                Some(b't') => self.literal("true", Value::Bool(true))?,
                Some(b'f') => self.literal("false", Value::Bool(false))?,
                Some(b'n') => self.literal("null", Value::Null)?,
                Some(b'-' | b'0'..=b'9') => self.number()?,
                _ => return self.fail("looking for beginning of value"),
            };
            // Attach the finished value to its parents, closing completed containers.
            loop {
                let Some(frame) = stack.last_mut() else {
                    return Ok(value);
                };
                self.space();
                match frame {
                    Frame::Array(items) => {
                        items.push(value);
                        match self.data.get(self.at) {
                            Some(b',') => {
                                self.at += 1;
                                break;
                            }
                            Some(b']') => {
                                self.at += 1;
                                let Some(Frame::Array(items)) = stack.pop() else {
                                    unreachable!()
                                };
                                value = Value::Array(items);
                            }
                            _ => return self.fail("after array element"),
                        }
                    }
                    Frame::Object(map, key) => {
                        map.insert(key.take().unwrap_or_default(), value);
                        match self.data.get(self.at) {
                            Some(b',') => {
                                self.at += 1;
                                self.space();
                                if self.data.get(self.at) != Some(&b'"') {
                                    return self.fail("looking for beginning of object key string");
                                }
                                let next = self.string()?;
                                self.space();
                                if self.data.get(self.at) != Some(&b':') {
                                    return self.fail("after object key");
                                }
                                self.at += 1;
                                let Some(Frame::Object(_, key)) = stack.last_mut() else {
                                    unreachable!()
                                };
                                *key = Some(next);
                                break;
                            }
                            Some(b'}') => {
                                self.at += 1;
                                let Some(Frame::Object(map, _)) = stack.pop() else {
                                    unreachable!()
                                };
                                value = Value::Object(map);
                            }
                            _ => return self.fail("after object key:value pair"),
                        }
                    }
                }
            }
        }
    }
}

/// Decodes the first JSON value of `data`: number literals stay verbatim
/// (`use_number`) or become f64, later duplicate keys win.
/// Returns the value and the number of bytes consumed.
pub fn parse_json(data: &[u8], use_number: bool) -> Result<(Value, usize)> {
    let mut parser = JsonParser {
        data,
        at: 0,
        use_number,
    };
    let value = parser.value()?;
    Ok((value, parser.at))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn floats_have_fixed_layouts() {
        for (value, sprint, json) in [
            (1.0, "1", "1"),
            (1e6, "1e+06", "1000000"),
            (1e20, "1e+20", "100000000000000000000"),
            (1e21, "1e+21", "1e+21"),
            (123456789.0, "1.23456789e+08", "123456789"),
            (0.0001, "0.0001", "0.0001"),
            (0.00001, "1e-05", "0.00001"),
            (1e-7, "1e-07", "1e-7"),
            (1.5, "1.5", "1.5"),
            (100000.0, "100000", "100000"),
            (1234567.5, "1.2345675e+06", "1234567.5"),
            (2.5e-5, "2.5e-05", "0.000025"),
            (1e22, "1e+22", "1e+22"),
            (
                12345678901234567890.0,
                "1.2345678901234567e+19",
                "12345678901234567000",
            ),
            (-0.5, "-0.5", "-0.5"),
        ] {
            assert_eq!(float_layout(value), sprint);
            assert_eq!(json_float(value).unwrap(), json);
        }
    }
    #[test]
    fn strings_and_decimals_have_fixed_text() {
        let mut out = String::new();
        encode_string("<&>\u{2028}\u{7f}\u{8}\u{c}\u{1}é", &mut out);
        assert_eq!(out, "\"\\u003c\\u0026\\u003e\\u2028\u{7f}\\b\\f\\u0001é\"");
        assert_eq!(decimal("1e+21"), decimal("1000000000000000000000"));
        assert_eq!(decimal("0.50"), decimal("5e-1"));
        assert_eq!(decimal("-0"), decimal("0"));
        assert!(decimal("inf").is_none() && decimal("").is_none());
        assert_eq!(decimal("1.0").unwrap().int64(), Some(1));
        assert_eq!(decimal("1.5").unwrap().int64(), None);
        assert_eq!(
            sprint(&Value::Object(map([
                ("b", Value::Int(1)),
                ("a", Value::Array(vec![Value::Null, Value::Float(1.5)]))
            ]))),
            "map[a:[<nil> 1.5] b:1]"
        );
        assert_eq!(Value::Number(String::new()).to_json().unwrap(), "0");
    }
}
