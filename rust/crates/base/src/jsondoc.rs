//! JSON behaviour needed for persisted files and wire messages:
//! document-ordered members with their raw bytes, duplicate keys,
//! case-insensitive struct fields, U+FFFD repair, f64 and byte-vector rules,
//! sorted and HTML-escaped output.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Map, Number, Value};

/// Documents nested deeper than this bound (far below the 10000 levels a
/// reference decoder accepts) are refused so recursive consumers of the decoded
/// tree cannot exhaust a thread stack.
pub const MAX_DEPTH: usize = 512;

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub key: String,
    pub value: Json,
    /// Byte range of the value in the parsed input (its raw JSON).
    pub start: usize,
    pub end: usize,
}
impl Member {
    pub fn raw<'a>(&self, source: &'a [u8]) -> &'a [u8] {
        &source[self.start..self.end]
    }
}
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<Member>),
}
impl Json {
    /// Value description used in type-mismatch errors.
    pub fn kind(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "bool",
            Json::Number(_) => "number",
            Json::String(_) => "string",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }
    pub fn members(&self) -> Option<&[Member]> {
        match self {
            Json::Object(members) => Some(members),
            _ => None,
        }
    }
}

/// Checks that token-mode consumers apply while streaming, reported in stream
/// order: the deepest value allowed and whether duplicate keys are refused.
#[derive(Debug, Clone, Copy)]
pub struct TokenRules {
    pub max_depth: usize,
    pub nesting_error: &'static str,
    pub duplicate_error: &'static str,
}
struct Parser<'a> {
    data: &'a [u8],
    position: usize,
    decoder: bool,
    tokens: Option<TokenRules>,
}
/// The error for truncated input.
pub const END: &str = "unexpected end of JSON input";
fn quote_char(byte: u8) -> String {
    match byte {
        b'\'' => r"'\''".into(),
        b'"' => "'\"'".into(),
        7 => r"'\a'".into(),
        8 => r"'\b'".into(),
        12 => r"'\f'".into(),
        b'\n' => r"'\n'".into(),
        b'\r' => r"'\r'".into(),
        b'\t' => r"'\t'".into(),
        11 => r"'\v'".into(),
        b'\\' => r"'\\'".into(),
        0..=0x1f | 0x7f => format!("'\\x{byte:02x}'"),
        0x80..=0xa0 | 0xad => format!("'\\u{byte:04x}'"),
        _ => format!("'{}'", byte as char),
    }
}
fn invalid(byte: u8, context: &str) -> String {
    format!("invalid character {} {context}", quote_char(byte))
}
impl Parser<'_> {
    fn space(&mut self) {
        while matches!(
            self.data.get(self.position),
            Some(b' ' | b'\t' | b'\r' | b'\n')
        ) {
            self.position += 1;
        }
    }
    // How each reading mode reports truncated input: token mode tells a gap
    // between tokens from a cut value, stream mode only empty from cut input.
    fn end(&self, inside: bool) -> String {
        let text = match (self.tokens.is_some(), self.decoder) {
            (true, _) if inside => "unexpected EOF",
            (true, _) => "EOF",
            (false, true) if rest_is_space(self.data, 0) => "EOF",
            (false, true) => "unexpected EOF",
            (false, false) => END,
        };
        text.into()
    }
    fn peek(&self) -> Result<u8, String> {
        let byte = self.data.get(self.position).copied();
        byte.ok_or_else(|| self.end(false))
    }
    fn inside(&self, at: usize) -> Result<u8, String> {
        self.data.get(at).copied().ok_or_else(|| self.end(true))
    }
    // Inside a literal, number or escape. Whole-document mode treats the end as a
    // final space there and reports that character instead of the end.
    fn scalar(&self, at: usize) -> Result<u8, String> {
        match self.data.get(at) {
            Some(&byte) => Ok(byte),
            None if self.tokens.is_none() && !self.decoder => Ok(b' '),
            None => Err(self.end(true)),
        }
    }
    fn bounded(&self, depth: usize) -> Result<(), String> {
        match self.tokens {
            Some(rules) if depth > rules.max_depth => Err(rules.nesting_error.into()),
            _ => Ok(()),
        }
    }
    fn value(&mut self, depth: usize) -> Result<Json, String> {
        self.bounded(depth)?;
        self.space();
        let byte = self.peek()?;
        match byte {
            b'{' | b'[' if depth >= MAX_DEPTH => Err("exceeded max depth".into()),
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Json::String),
            b'-' | b'0'..=b'9' => self.number().map(Json::Number),
            b't' => self.literal("true", Json::Bool(true)),
            b'f' => self.literal("false", Json::Bool(false)),
            b'n' => self.literal("null", Json::Null),
            _ => Err(invalid(byte, "looking for beginning of value")),
        }
    }
    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.position += 1;
        let mut members = Vec::new();
        self.space();
        if self.peek()? == b'}' {
            self.position += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.space();
            let byte = self.peek()?;
            if byte != b'"' {
                // The token reader names no context for a bad first key.
                if self.tokens.is_some() && members.is_empty() {
                    return Err(format!("invalid character {}", quote_char(byte)));
                }
                return Err(invalid(byte, "looking for beginning of object key string"));
            }
            let key = self.string()?;
            if let Some(rules) = self.tokens {
                if members.iter().any(|member: &Member| member.key == key) {
                    return Err(rules.duplicate_error.into());
                }
                self.bounded(depth + 1)?;
            }
            self.space();
            let byte = self.peek()?;
            if byte != b':' {
                return Err(invalid(byte, "after object key"));
            }
            self.position += 1;
            self.space();
            let start = self.position;
            let value = self.value(depth + 1)?;
            members.push(Member {
                key,
                value,
                start,
                end: self.position,
            });
            self.space();
            match self.peek()? {
                b',' => self.position += 1,
                b'}' => {
                    self.position += 1;
                    return Ok(Json::Object(members));
                }
                byte => return Err(invalid(byte, "after object key:value pair")),
            }
        }
    }
    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.position += 1;
        let mut items = Vec::new();
        self.space();
        if self.peek()? == b']' {
            self.position += 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.space();
            match self.peek()? {
                b',' => self.position += 1,
                b']' => {
                    self.position += 1;
                    return Ok(Json::Array(items));
                }
                byte => return Err(invalid(byte, "after array element")),
            }
        }
    }
    fn literal(&mut self, text: &str, value: Json) -> Result<Json, String> {
        for (index, expected) in text.bytes().enumerate().skip(1) {
            let byte = self.scalar(self.position + index)?;
            if byte != expected {
                return Err(invalid(
                    byte,
                    &format!("in literal {text} (expecting '{}')", expected as char),
                ));
            }
        }
        self.position += text.len();
        Ok(value)
    }
    fn digits(&mut self) -> usize {
        let start = self.position;
        while matches!(self.data.get(self.position), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        self.position - start
    }
    fn number(&mut self) -> Result<String, String> {
        let start = self.position;
        if self.data[self.position] == b'-' {
            self.position += 1;
        }
        match self.scalar(self.position)? {
            b'0' => self.position += 1,
            b'1'..=b'9' => {
                self.digits();
            }
            byte => return Err(invalid(byte, "in numeric literal")),
        }
        if self.data.get(self.position) == Some(&b'.') {
            self.position += 1;
            if self.digits() == 0 {
                return Err(invalid(
                    self.scalar(self.position)?,
                    "after decimal point in numeric literal",
                ));
            }
        }
        if matches!(self.data.get(self.position), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.data.get(self.position), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if self.digits() == 0 {
                return Err(invalid(
                    self.scalar(self.position)?,
                    "in exponent of numeric literal",
                ));
            }
        }
        Ok(String::from_utf8_lossy(&self.data[start..self.position]).into_owned())
    }
    fn hex4(&self, at: usize) -> Option<u32> {
        let digits = self.data.get(at..at + 4)?;
        let mut value = 0;
        for &digit in digits {
            value = value * 16 + (digit as char).to_digit(16)?;
        }
        Some(value)
    }
    fn string(&mut self) -> Result<String, String> {
        self.position += 1;
        let mut text = String::new();
        loop {
            let start = self.position;
            while !matches!(
                self.data.get(self.position),
                None | Some(b'"' | b'\\' | 0..=0x1f)
            ) {
                self.position += 1;
            }
            push_repaired(&mut text, &self.data[start..self.position]);
            match self.inside(self.position)? {
                b'"' => {
                    self.position += 1;
                    return Ok(text);
                }
                b'\\' => {
                    self.position += 1;
                    let escape = self.scalar(self.position)?;
                    self.position += 1;
                    match escape {
                        b'"' | b'\\' | b'/' => text.push(escape as char),
                        b'b' => text.push('\u{8}'),
                        b'f' => text.push('\u{c}'),
                        b'n' => text.push('\n'),
                        b'r' => text.push('\r'),
                        b't' => text.push('\t'),
                        b'u' => {
                            let unit = self.unit()?;
                            self.position += 4;
                            text.push(self.rune(unit));
                        }
                        _ => return Err(invalid(escape, "in string escape code")),
                    }
                }
                byte => return Err(invalid(byte, "in string literal")),
            }
        }
    }
    fn unit(&self) -> Result<u32, String> {
        for offset in 0..4 {
            let byte = self.scalar(self.position + offset)?;
            if !byte.is_ascii_hexdigit() {
                return Err(invalid(byte, "in \\u hexadecimal character escape"));
            }
        }
        Ok(self.hex4(self.position).unwrap())
    }
    // An unpaired surrogate becomes U+FFFD and leaves the following escape alone.
    fn rune(&mut self, unit: u32) -> char {
        if (0xd800..0xdc00).contains(&unit)
            && self.data.get(self.position..self.position + 2) == Some(b"\\u")
            && let Some(low) = self.hex4(self.position + 2)
            && (0xdc00..0xe000).contains(&low)
        {
            self.position += 6;
            return char::from_u32(0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00))
                .unwrap_or('\u{fffd}');
        }
        char::from_u32(unit).unwrap_or('\u{fffd}')
    }
}
// Every invalid byte is replaced with its own U+FFFD.
fn push_repaired(text: &mut String, mut bytes: &[u8]) {
    loop {
        match std::str::from_utf8(bytes) {
            Ok(valid) => {
                text.push_str(valid);
                return;
            }
            Err(error) => {
                let (valid, rest) = bytes.split_at(error.valid_up_to());
                text.push_str(std::str::from_utf8(valid).unwrap());
                text.push('\u{fffd}');
                bytes = &rest[1..];
            }
        }
    }
}

/// Stream mode: the first value and the offset after it.
pub fn parse_first(data: &[u8]) -> Result<(Json, usize), String> {
    let mut parser = Parser {
        data,
        position: 0,
        decoder: true,
        tokens: None,
    };
    let value = parser.value(0)?;
    Ok((value, parser.position))
}
/// True when only whitespace follows `from`, so a second stream read would hit the end.
pub fn rest_is_space(data: &[u8], from: usize) -> bool {
    data[from..]
        .iter()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
}
/// One value read token by token (token mode): the same grammar, the token
/// reader's error texts, and `rules` enforced in stream order.
pub fn parse_tokens(data: &[u8], rules: TokenRules) -> Result<(Json, usize), String> {
    let mut parser = Parser {
        data,
        position: 0,
        decoder: false,
        tokens: Some(rules),
    };
    let value = parser.value(0)?;
    Ok((value, parser.position))
}
/// Whole-document syntax rules: exactly one value.
pub fn parse(data: &[u8]) -> Result<Json, String> {
    let mut parser = Parser {
        data,
        position: 0,
        decoder: false,
        tokens: None,
    };
    let value = parser.value(0)?;
    parser.space();
    match data.get(parser.position) {
        None => Ok(value),
        Some(&byte) => Err(invalid(byte, "after top-level value")),
    }
}
pub fn valid(data: &[u8]) -> bool {
    parse(data).is_ok()
}

fn fold(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\u{212a}' => 'k',
            '\u{17f}' => 's',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}
/// Applies object members to struct fields: in document order,
/// an exact name first, otherwise a case-insensitive one; unknown keys are
/// skipped; decoding continues after an error and the first error is returned.
pub fn decode_struct(
    members: &[Member],
    fields: &[&str],
    mut set: impl FnMut(usize, &Member) -> Result<(), String>,
) -> Result<(), String> {
    let mut first = None;
    for member in members {
        let field = fields
            .iter()
            .position(|field| *field == member.key)
            .or_else(|| {
                let folded = fold(&member.key);
                fields.iter().position(|field| fold(field) == folded)
            });
        if let Some(field) = field
            && let Err(error) = set(field, member)
        {
            first.get_or_insert(error);
        }
    }
    first.map_or(Ok(()), Err)
}
/// Target description for type errors: a top-level value or a struct field.
#[derive(Clone, Copy)]
pub struct Target<'a> {
    pub strukt: &'a str,
    pub field: &'a str,
}
impl Target<'_> {
    pub const VALUE: Target<'static> = Target {
        strukt: "",
        field: "",
    };
    pub fn mismatch(&self, value: &str, target_type: &str) -> String {
        if self.field.is_empty() {
            format!("json: cannot unmarshal {value} into value of type {target_type}")
        } else {
            format!(
                "json: cannot unmarshal {value} into field {}.{} of type {target_type}",
                self.strukt, self.field
            )
        }
    }
}
// `null` leaves every target unchanged.
pub fn set_string(target: &mut String, value: &Json, at: Target<'_>) -> Result<(), String> {
    match value {
        Json::Null => {}
        Json::String(text) => *target = text.clone(),
        other => return Err(at.mismatch(other.kind(), "string")),
    }
    Ok(())
}
pub fn set_int(target: &mut i64, value: &Json, at: Target<'_>) -> Result<(), String> {
    match value {
        Json::Null => {}
        Json::Number(text) => {
            *target = text
                .parse()
                .map_err(|_| at.mismatch(&format!("number {text}"), "int"))?
        }
        other => return Err(at.mismatch(other.kind(), "int")),
    }
    Ok(())
}
pub fn set_uint64(target: &mut u64, value: &Json, at: Target<'_>) -> Result<(), String> {
    match value {
        Json::Null => {}
        Json::Number(text) => {
            *target = text
                .parse()
                .map_err(|_| at.mismatch(&format!("number {text}"), "uint64"))?
        }
        other => return Err(at.mismatch(other.kind(), "uint64")),
    }
    Ok(())
}
pub fn set_bool(target: &mut bool, value: &Json, at: Target<'_>) -> Result<(), String> {
    match value {
        Json::Null => {}
        Json::Bool(flag) => *target = *flag,
        other => return Err(at.mismatch(other.kind(), "bool")),
    }
    Ok(())
}
/// A byte vector: standard base64 with embedded CR/LF ignored; `null` yields `None`.
pub fn set_bytes(target: &mut Option<Vec<u8>>, value: &Json, at: Target<'_>) -> Result<(), String> {
    match value {
        Json::Null => *target = None,
        Json::String(text) => {
            let text: String = text.chars().filter(|c| !matches!(c, '\r' | '\n')).collect();
            *target = Some(
                STANDARD
                    .decode(text)
                    .map_err(|_| "illegal base64 data".to_string())?,
            )
        }
        other => return Err(at.mismatch(other.kind(), "[]uint8")),
    }
    Ok(())
}

/// JSON text of an f64: shortest round-trip form, exponent form outside
/// [1e-6, 1e21). Fails for literals beyond the f64 range.
pub fn float_text(literal: &str) -> Result<String, String> {
    let number: f64 = literal
        .parse()
        .ok()
        .filter(|number: &f64| number.is_finite())
        .ok_or_else(|| Target::VALUE.mismatch(&format!("number {literal}"), "float64"))?;
    let magnitude = number.abs();
    if magnitude != 0.0 && !(1e-6..1e21).contains(&magnitude) {
        let text = format!("{number:e}");
        let (mantissa, exponent) = text.split_once('e').unwrap();
        let exponent: i32 = exponent.parse().unwrap();
        let sign = if exponent < 0 { '-' } else { '+' };
        return Ok(format!("{mantissa}e{sign}{}", exponent.abs()));
    }
    Ok(format!("{number}"))
}
/// Decodes into a value tree: numbers verbatim or as f64 text, the
/// last duplicate key wins.
pub fn to_value(json: &Json, float: bool) -> Result<Value, String> {
    Ok(match json {
        Json::Null => Value::Null,
        Json::Bool(flag) => Value::Bool(*flag),
        Json::Number(text) => {
            let text = if float {
                float_text(text)?
            } else {
                text.clone()
            };
            Value::Number(
                // The literal came from the parser above. Every public constructor
                // renormalizes integers and would turn `-0` into `0`.
                Number::from_string_unchecked(text),
            )
        }
        Json::String(text) => Value::String(text.clone()),
        Json::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| to_value(item, float))
                .collect::<Result<_, _>>()?,
        ),
        Json::Object(members) => Value::Object(to_map(members, float)?),
    })
}
pub fn to_map(members: &[Member], float: bool) -> Result<Map<String, Value>, String> {
    let mut map = Map::new();
    for member in members {
        map.insert(member.key.clone(), to_value(&member.value, float)?);
    }
    Ok(map)
}

/// A JSON string literal, including HTML and line-separator escapes.
pub fn quote(text: &str) -> String {
    escape_html(serde_json::to_string(text).unwrap())
}
fn escape_html(text: String) -> String {
    if !text.contains(['&', '<', '>', '\u{2028}', '\u{2029}']) {
        return text;
    }
    text.replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}
/// Compact JSON for serde values: struct fields in declaration order, map
/// keys sorted, HTML-sensitive characters escaped. These characters occur only
/// inside strings.
pub fn marshal<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String> {
    serde_json::to_string(value)
        .map(|text| escape_html(text).into_bytes())
        .map_err(|error| error.to_string())
}
/// Like [`marshal`], indented by two spaces.
pub fn marshal_indent<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String> {
    serde_json::to_string_pretty(value)
        .map(|text| escape_html(text).into_bytes())
        .map_err(|error| error.to_string())
}
/// [`marshal`] of a decoded value tree.
pub fn encode(value: &Value) -> Vec<u8> {
    marshal(value).expect("JSON values always serialize")
}
/// What [`marshal`] emits for valid raw JSON: insignificant
/// whitespace removed and HTML-sensitive characters escaped, otherwise verbatim.
pub fn compact(raw: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(raw.len());
    let (mut in_string, mut escaped) = (false, false);
    let mut index = 0;
    while index < raw.len() {
        let byte = raw[index];
        index += 1;
        match byte {
            b'<' | b'>' | b'&' => {
                result.extend_from_slice(format!("\\u00{byte:02x}").as_bytes());
                escaped = false;
                continue;
            }
            0xe2 if raw[index..].starts_with(&[0x80, 0xa8]) => {
                result.extend_from_slice(b"\\u2028");
                index += 2;
                escaped = false;
                continue;
            }
            0xe2 if raw[index..].starts_with(&[0x80, 0xa9]) => {
                result.extend_from_slice(b"\\u2029");
                index += 2;
                escaped = false;
                continue;
            }
            _ => {}
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
            continue;
        } else if byte == b'"' {
            in_string = true;
        }
        result.push(byte);
    }
    result
}
// Non-printable characters in non-ASCII text: separators, controls, format and
// private-use characters are escaped. Unassigned code points are not known here.
fn unprintable(character: char) -> bool {
    character.is_control()
        || character.is_whitespace()
        || matches!(
            character as u32,
            0xad | 0x600..=0x605
                | 0x61c
                | 0x6dd
                | 0x70f
                | 0x8e2
                | 0x180e
                | 0x200b..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x2064
                | 0x2066..=0x206f
                | 0xe000..=0xf8ff
                | 0xfeff
                | 0xfff9..=0xfffb
                | 0xfffe..=0xffff
                | 0xe0001
                | 0xe0020..=0xe007f
                | 0xf0000..=0x10ffff
        )
}
/// A double-quoted string with escapes, for the strings these modules quote in messages.
pub fn quoted_text(text: &str) -> String {
    let mut result = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\u{7}' => result.push_str("\\a"),
            '\u{8}' => result.push_str("\\b"),
            '\u{c}' => result.push_str("\\f"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\u{b}' => result.push_str("\\v"),
            other if (other as u32) < 0x20 || other == '\u{7f}' => {
                result.push_str(&format!("\\x{:02x}", other as u32))
            }
            ' ' => result.push(' '),
            other if unprintable(other) && (other as u32) < 0x10000 => {
                result.push_str(&format!("\\u{:04x}", other as u32))
            }
            other if unprintable(other) => result.push_str(&format!("\\U{:08x}", other as u32)),
            other => result.push(other),
        }
    }
    result.push('"');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_acceptance_and_messages() {
        for (input, message) in [
            ("", END),
            ("  ", END),
            ("{", END),
            ("\"a", END),
            ("[1,", END),
            (
                "tru",
                "invalid character ' ' in literal true (expecting 'e')",
            ),
            ("-", "invalid character ' ' in numeric literal"),
            (
                "1e+",
                "invalid character ' ' in exponent of numeric literal",
            ),
            ("\"\\", "invalid character ' ' in string escape code"),
            (
                "\"\\u12",
                "invalid character ' ' in \\u hexadecimal character escape",
            ),
            (
                "[1,]",
                "invalid character ']' looking for beginning of value",
            ),
            (
                "{\"a\":1,}",
                "invalid character '}' looking for beginning of object key string",
            ),
            ("{\"a\" 1}", "invalid character '1' after object key"),
            (
                "{\"a\":1 \"b\":2}",
                "invalid character '\"' after object key:value pair",
            ),
            ("[1 2]", "invalid character '2' after array element"),
            ("01", "invalid character '1' after top-level value"),
            ("-x", "invalid character 'x' in numeric literal"),
            (
                "1.e3",
                "invalid character 'e' after decimal point in numeric literal",
            ),
            (
                "1ex",
                "invalid character 'x' in exponent of numeric literal",
            ),
            (
                "tru3",
                "invalid character '3' in literal true (expecting 'e')",
            ),
            ("\"a\nb\"", "invalid character '\\n' in string literal"),
            ("\"\\q\"", "invalid character 'q' in string escape code"),
            (
                "\"\\u12g4\"",
                "invalid character 'g' in \\u hexadecimal character escape",
            ),
            ("{} {}", "invalid character '{' after top-level value"),
            (
                "'a'",
                "invalid character '\\'' looking for beginning of value",
            ),
        ] {
            assert_eq!(parse(input.as_bytes()).unwrap_err(), message, "{input:?}");
        }
        assert!(valid(
            b" {\"a\":[1,-0,1.5e+3,true,false,null,\"\\u00e9\\/\"]}\r\n"
        ));
        let nested = |depth: usize| format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        assert!(valid(nested(MAX_DEPTH).as_bytes()));
        assert_eq!(
            parse(nested(MAX_DEPTH + 1).as_bytes()).unwrap_err(),
            "exceeded max depth"
        );
    }

    #[test]
    fn strings_repair_invalid_text() {
        let parsed = parse(b"\"\xff\xfe\\ud800x\\ud83d\\ude00\\udc00\\ud800\\u0041\"").unwrap();
        assert_eq!(
            parsed.as_str().unwrap(),
            "\u{fffd}\u{fffd}\u{fffd}x\u{1f600}\u{fffd}\u{fffd}A"
        );
    }

    #[test]
    fn members_keep_document_order_raw_bytes_and_duplicates() {
        let source = br#"{"b": [1, 2] ,"a":{"x":1.0},"b":null}"#;
        let (parsed, end) = parse_first(source).unwrap();
        assert!(rest_is_space(source, end));
        let members = parsed.members().unwrap();
        assert_eq!(
            members.iter().map(|m| m.key.as_str()).collect::<Vec<_>>(),
            ["b", "a", "b"]
        );
        assert_eq!(members[0].raw(source), b"[1, 2]");
        assert_eq!(members[1].raw(source), br#"{"x":1.0}"#);
        assert_eq!(
            encode(&to_value(&parsed, false).unwrap()),
            br#"{"a":{"x":1.0},"b":null}"#
        );
        assert_eq!(
            encode(&to_value(&parsed, true).unwrap()),
            br#"{"a":{"x":1},"b":null}"#
        );
    }

    #[test]
    fn struct_fields_match_exactly_then_case_insensitively() {
        let parsed = parse(br#"{"PID":1,"pid":2,"Pid":"x","other":[],"flag":null}"#).unwrap();
        let (mut pid, mut flag) = (0, true);
        let error = decode_struct(
            parsed.members().unwrap(),
            &["pid", "flag"],
            |field, member| {
                let at = Target {
                    strukt: "record",
                    field: ["pid", "flag"][field],
                };
                match field {
                    0 => set_int(&mut pid, &member.value, at),
                    _ => set_bool(&mut flag, &member.value, at),
                }
            },
        )
        .unwrap_err();
        assert_eq!((pid, flag), (2, true));
        assert_eq!(
            error,
            "json: cannot unmarshal string into field record.pid of type int"
        );
        let mut value = 0;
        assert_eq!(
            set_int(&mut value, &Json::Number("1.0".into()), Target::VALUE).unwrap_err(),
            "json: cannot unmarshal number 1.0 into value of type int"
        );
        let mut bytes = None;
        set_bytes(&mut bytes, &Json::String("e30=\r\n".into()), Target::VALUE).unwrap();
        assert_eq!(bytes.as_deref(), Some(&b"{}"[..]));
    }

    #[test]
    fn floats_and_output_formatting() {
        for (literal, text) in [
            ("1.0", "1"),
            ("-0", "-0"),
            ("0.000001", "0.000001"),
            ("0.0000001", "1e-7"),
            ("1e21", "1e+21"),
            ("123456789012345678901", "123456789012345680000"),
            ("12345678901234567890123", "1.2345678901234568e+22"),
            ("1.5e-10", "1.5e-10"),
            ("1E2", "100"),
            ("1e-400", "0"),
            ("5e-324", "5e-324"),
            ("1.7976931348623157e308", "1.7976931348623157e+308"),
        ] {
            assert_eq!(float_text(literal).unwrap(), text, "{literal}");
        }
        let literals = parse(b"[-0,1E3,2.50,0e0,18446744073709551616]").unwrap();
        assert_eq!(
            encode(&to_value(&literals, false).unwrap()),
            b"[-0,1E3,2.50,0e0,18446744073709551616]"
        );
        assert_eq!(
            encode(&to_value(&literals, true).unwrap()),
            b"[-0,1000,2.5,0,18446744073709552000]"
        );
        assert_eq!(
            float_text("1e999").unwrap_err(),
            "json: cannot unmarshal number 1e999 into value of type float64"
        );
        assert_eq!(
            quote("a<b>&\u{2028}\"\u{1}\u{8}é"),
            "\"a\\u003cb\\u003e\\u0026\\u2028\\\"\\u0001\\bé\""
        );
        assert_eq!(
            compact(b" { \"a<\" : [ 1 , \"x y\\\" \xe2\x80\xa9\" ] } \n"),
            b"{\"a\\u003c\":[1,\"x y\\\" \\u2029\"]}"
        );
        assert_eq!(quoted_text("a\"b\\\n\u{1}é"), "\"a\\\"b\\\\\\n\\x01é\"");
        assert_eq!(
            quoted_text("\u{2028}\u{a0}\u{fffd} \u{200b}\u{e000}\u{10ffff}"),
            "\"\\u2028\\u00a0\u{fffd} \\u200b\\ue000\\U0010ffff\""
        );
        let rules = TokenRules {
            max_depth: 2,
            nesting_error: "deep",
            duplicate_error: "twice",
        };
        let cut = |data: &[u8]| {
            (
                parse_first(data).err().unwrap_or_default(),
                parse_tokens(data, rules).err().unwrap_or_default(),
            )
        };
        assert_eq!(cut(b" "), ("EOF".to_string(), "EOF".to_string()));
        assert_eq!(
            cut(b"{\"a\":[1,"),
            ("unexpected EOF".to_string(), "EOF".to_string())
        );
        assert_eq!(
            cut(b"{\"a\":tr"),
            ("unexpected EOF".to_string(), "unexpected EOF".to_string())
        );
        assert_eq!(cut(b"{a:1}").1, "invalid character 'a'");
        assert_eq!(cut(b"{\"a\":1,\"a\":[[]]}").1, "twice");
        assert_eq!(cut(b"{\"a\":[[[]]]}").1, "deep");
    }
}
