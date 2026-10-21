//! Command-line flag parsing: single or double dash, `-flag=value`, boolean flags without a value.
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Text(String),
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    /// A duration in nanoseconds.
    Duration(i64),
    /// A repeatable value; the checker validates each new value and sees the values so far.
    List(Vec<String>, Check),
}
#[derive(Clone, Copy)]
struct Check(ListCheck);
impl std::fmt::Debug for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Check")
    }
}
impl PartialEq for Check {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
/// Validator of a repeatable flag: previous values and the new one.
pub type ListCheck = fn(&[String], &str) -> Result<(), String>;
#[derive(Debug, Clone)]
struct Flag {
    usage: String,
    default: Value,
    value: Value,
    set: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagError {
    /// `-h`/`-help` was given and is not a defined flag.
    Help,
    Invalid(String),
}
impl std::fmt::Display for FlagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Help => f.write_str("flag: help requested"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for FlagError {}

/// A flag set: `-flag`, `--flag`, `-flag=value`, `-flag value`, booleans without
/// a value, parsing stops at the first non-flag argument or after `--`.
#[derive(Debug, Clone)]
pub struct FlagSet {
    name: String,
    flags: BTreeMap<String, Flag>,
    args: Vec<String>,
    quiet: bool,
}
impl FlagSet {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            flags: BTreeMap::new(),
            args: Vec::new(),
            quiet: false,
        }
    }
    /// Suppress the error/usage text that parsing otherwise writes to stderr.
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }
    fn define(&mut self, name: &str, default: Value, usage: &str) {
        let previous = self.flags.insert(
            name.into(),
            Flag {
                usage: usage.into(),
                value: default.clone(),
                default,
                set: false,
            },
        );
        assert!(previous.is_none(), "flag redefined: {name}");
    }
    pub fn string(&mut self, name: &str, default: &str, usage: &str) {
        self.define(name, Value::Text(default.into()), usage);
    }
    pub fn bool(&mut self, name: &str, default: bool, usage: &str) {
        self.define(name, Value::Bool(default), usage);
    }
    pub fn int(&mut self, name: &str, default: i64, usage: &str) {
        self.define(name, Value::Int(default), usage);
    }
    pub fn uint(&mut self, name: &str, default: u64, usage: &str) {
        self.define(name, Value::Uint(default), usage);
    }
    pub fn float(&mut self, name: &str, default: f64, usage: &str) {
        self.define(name, Value::Float(default), usage);
    }
    pub fn duration(&mut self, name: &str, default: std::time::Duration, usage: &str) {
        self.define(name, Value::Duration(default.as_nanos() as i64), usage);
    }
    /// Defines a flag that may be repeated.
    pub fn list(&mut self, name: &str, check: ListCheck, usage: &str) {
        self.define(name, Value::List(Vec::new(), Check(check)), usage);
    }
    fn value(&self, name: &str) -> &Value {
        &self
            .flags
            .get(name)
            .unwrap_or_else(|| panic!("flag accessed but not defined: {name}"))
            .value
    }
    pub fn get_string(&self, name: &str) -> String {
        match self.value(name) {
            Value::Text(value) => value.clone(),
            _ => panic!("flag {name} is not a string"),
        }
    }
    pub fn get_bool(&self, name: &str) -> bool {
        match self.value(name) {
            Value::Bool(value) => *value,
            _ => panic!("flag {name} is not a bool"),
        }
    }
    pub fn get_int(&self, name: &str) -> i64 {
        match self.value(name) {
            Value::Int(value) => *value,
            _ => panic!("flag {name} is not an int"),
        }
    }
    pub fn get_uint(&self, name: &str) -> u64 {
        match self.value(name) {
            Value::Uint(value) => *value,
            _ => panic!("flag {name} is not a uint"),
        }
    }
    pub fn get_float(&self, name: &str) -> f64 {
        match self.value(name) {
            Value::Float(value) => *value,
            _ => panic!("flag {name} is not a float"),
        }
    }
    /// Nanoseconds; a duration may be negative.
    pub fn get_duration(&self, name: &str) -> i64 {
        match self.value(name) {
            Value::Duration(value) => *value,
            _ => panic!("flag {name} is not a duration"),
        }
    }
    pub fn get_list(&self, name: &str) -> Vec<String> {
        match self.value(name) {
            Value::List(values, _) => values.clone(),
            _ => panic!("flag {name} is not a list"),
        }
    }
    /// Reports whether the flag was given on the command line.
    pub fn is_set(&self, name: &str) -> bool {
        self.flags.get(name).is_some_and(|flag| flag.set)
    }
    /// Remaining non-flag arguments.
    pub fn args(&self) -> &[String] {
        &self.args
    }
    /// The default usage text: a header followed by every flag with its default.
    pub fn usage(&self) -> String {
        let mut text = if self.name.is_empty() {
            "Usage:\n".to_string()
        } else {
            format!("Usage of {}:\n", self.name)
        };
        for (name, flag) in &self.flags {
            let mut line = format!("  -{name}");
            let (kind, usage) = unquote_usage(flag);
            if !kind.is_empty() {
                line.push(' ');
                line.push_str(&kind);
            }
            line.push_str(if line.len() <= 4 { "\t" } else { "\n    \t" });
            line.push_str(&usage.replace('\n', "\n    \t"));
            match &flag.default {
                Value::Text(value) if !value.is_empty() => {
                    line.push_str(&format!(" (default {})", crate::flags::quote(value)))
                }
                Value::Bool(true) => line.push_str(" (default true)"),
                Value::Int(value) if *value != 0 => line.push_str(&format!(" (default {value})")),
                Value::Uint(value) if *value != 0 => line.push_str(&format!(" (default {value})")),
                Value::Float(value) if *value != 0.0 => {
                    line.push_str(&format!(" (default {value})"))
                }
                Value::Duration(value) if *value != 0 => {
                    line.push_str(&format!(" (default {})", duration_text(*value)))
                }
                _ => {}
            }
            text.push_str(&line);
            text.push('\n');
        }
        text
    }
    fn fail(&self, message: String) -> FlagError {
        if !self.quiet {
            eprintln!("{message}");
            eprint!("{}", self.usage());
        }
        FlagError::Invalid(message)
    }
    pub fn parse(&mut self, arguments: impl IntoIterator<Item = String>) -> Result<(), FlagError> {
        let mut pending: std::collections::VecDeque<String> = arguments.into_iter().collect();
        while let Some(argument) = pending.front() {
            let bytes = argument.as_bytes();
            if bytes.len() < 2 || bytes[0] != b'-' {
                break;
            }
            let mut minuses = 1;
            if bytes[1] == b'-' {
                minuses = 2;
                if bytes.len() == 2 {
                    pending.pop_front();
                    break;
                }
            }
            let mut name = &argument[minuses..];
            if name.is_empty() || name.starts_with('-') || name.starts_with('=') {
                return Err(self.fail(format!("bad flag syntax: {argument}")));
            }
            let mut value = None;
            if let Some(index) = name[1..].find('=') {
                value = Some(name[index + 2..].to_string());
                name = &name[..index + 1];
            }
            let name = name.to_string();
            pending.pop_front();
            let Some(flag) = self.flags.get(&name) else {
                if name == "help" || name == "h" {
                    if !self.quiet {
                        eprint!("{}", self.usage());
                    }
                    return Err(FlagError::Help);
                }
                return Err(self.fail(format!("flag provided but not defined: -{name}")));
            };
            let parsed = if matches!(flag.default, Value::Bool(_)) {
                match &value {
                    Some(text) => match parse_bool(text) {
                        Some(parsed) => Value::Bool(parsed),
                        None => {
                            return Err(self.fail(format!(
                                "invalid boolean value {} for -{name}: parse error",
                                quote(text)
                            )));
                        }
                    },
                    None => Value::Bool(true),
                }
            } else {
                let text = match value.or_else(|| pending.pop_front()) {
                    Some(text) => text,
                    None => return Err(self.fail(format!("flag needs an argument: -{name}"))),
                };
                let parsed = match flag.default {
                    Value::Text(_) => Ok(Value::Text(text.clone())),
                    Value::Int(_) => parse_int(&text).map(Value::Int),
                    Value::Uint(_) => parse_uint(&text).map(Value::Uint),
                    Value::Float(_) => text
                        .replace('_', "")
                        .parse::<f64>()
                        .map(Value::Float)
                        .map_err(|_| "parse error"),
                    Value::Duration(_) => parse_duration(&text)
                        .map(Value::Duration)
                        .ok_or("parse error"),
                    Value::List(_, check) => match &self.flags[&name].value {
                        Value::List(values, _) => match (check.0)(values, &text) {
                            Ok(()) => {
                                let mut values = values.clone();
                                values.push(text.clone());
                                Ok(Value::List(values, check))
                            }
                            Err(reason) => {
                                return Err(self.fail(format!(
                                    "invalid value {} for flag -{name}: {reason}",
                                    quote(&text)
                                )));
                            }
                        },
                        _ => unreachable!(),
                    },
                    Value::Bool(_) => unreachable!(),
                };
                match parsed {
                    Ok(parsed) => parsed,
                    Err(reason) => {
                        return Err(self.fail(format!(
                            "invalid value {} for flag -{name}: {reason}",
                            quote(&text)
                        )));
                    }
                }
            };
            let flag = self.flags.get_mut(&name).unwrap();
            flag.value = parsed;
            flag.set = true;
        }
        self.args = pending.into();
        Ok(())
    }
}
fn unquote_usage(flag: &Flag) -> (String, String) {
    if let Some(start) = flag.usage.find('`')
        && let Some(length) = flag.usage[start + 1..].find('`')
    {
        let name = &flag.usage[start + 1..start + 1 + length];
        let usage = format!(
            "{}{name}{}",
            &flag.usage[..start],
            &flag.usage[start + 2 + length..]
        );
        return (name.into(), usage);
    }
    let kind = match flag.default {
        Value::Text(_) => "string",
        Value::Bool(_) => "",
        Value::Int(_) => "int",
        Value::Uint(_) => "uint",
        Value::Float(_) => "float",
        Value::Duration(_) => "duration",
        Value::List(..) => "value",
    };
    (kind.into(), flag.usage.clone())
}
fn parse_bool(text: &str) -> Option<bool> {
    match text {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}
/// Unsigned integer with base prefixes (`0x`, `0b`, `0o`, leading `0`) and digit-separating underscores.
fn parse_uint(text: &str) -> Result<u64, &'static str> {
    let lower = text.to_ascii_lowercase();
    let (radix, digits, prefixed) = if let Some(rest) = lower.strip_prefix("0x") {
        (16, rest, true)
    } else if let Some(rest) = lower.strip_prefix("0b") {
        (2, rest, true)
    } else if let Some(rest) = lower.strip_prefix("0o") {
        (8, rest, true)
    } else if lower.len() > 1 && lower.starts_with('0') {
        (8, &lower[1..], true)
    } else {
        (10, lower.as_str(), false)
    };
    let bytes = digits.as_bytes();
    let valid_underscores = bytes.iter().enumerate().all(|(index, &byte)| {
        byte != b'_'
            || ((index > 0 || prefixed)
                && index + 1 < bytes.len()
                && bytes[index + 1] != b'_'
                && (index == 0 || bytes[index - 1] != b'_'))
    });
    let clean = digits.replace('_', "");
    if clean.is_empty() || !valid_underscores || clean.starts_with(['+', '-']) {
        return Err("parse error");
    }
    u64::from_str_radix(&clean, radix).map_err(|error| match error.kind() {
        std::num::IntErrorKind::PosOverflow => "value out of range",
        _ => "parse error",
    })
}
fn parse_int(text: &str) -> Result<i64, &'static str> {
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let magnitude = parse_uint(digits)?;
    if negative {
        if magnitude > 1 << 63 {
            return Err("value out of range");
        }
        Ok((magnitude as i64).wrapping_neg())
    } else {
        i64::try_from(magnitude).map_err(|_| "value out of range")
    }
}
/// Parses a duration such as `1h30m` or `1.5s`, in nanoseconds.
pub fn parse_duration(text: &str) -> Option<i64> {
    let mut rest = text.as_bytes();
    let mut negative = false;
    if let Some(&sign) = rest.first()
        && (sign == b'-' || sign == b'+')
    {
        negative = sign == b'-';
        rest = &rest[1..];
    }
    if rest == b"0" {
        return Some(0);
    }
    if rest.is_empty() {
        return None;
    }
    const LIMIT: u64 = 1 << 63;
    let mut total = 0u64;
    while !rest.is_empty() {
        if !(rest[0] == b'.' || rest[0].is_ascii_digit()) {
            return None;
        }
        let before = rest.len();
        let mut whole = 0u64;
        while let Some(&digit) = rest.first().filter(|digit| digit.is_ascii_digit()) {
            if whole > LIMIT / 10 {
                return None;
            }
            whole = whole * 10 + u64::from(digit - b'0');
            if whole > LIMIT {
                return None;
            }
            rest = &rest[1..];
        }
        let pre = before != rest.len();
        let (mut fraction, mut scale, mut post) = (0u64, 1f64, false);
        if rest.first() == Some(&b'.') {
            rest = &rest[1..];
            let before = rest.len();
            let mut overflow = false;
            while let Some(&digit) = rest.first().filter(|digit| digit.is_ascii_digit()) {
                rest = &rest[1..];
                if overflow {
                    continue;
                }
                if fraction > (LIMIT - 1) / 10 {
                    overflow = true;
                    continue;
                }
                let next = fraction * 10 + u64::from(digit - b'0');
                if next > LIMIT {
                    overflow = true;
                    continue;
                }
                fraction = next;
                scale *= 10.0;
            }
            post = before != rest.len();
        }
        if !pre && !post {
            return None;
        }
        let length = rest
            .iter()
            .position(|&byte| byte == b'.' || byte.is_ascii_digit())
            .unwrap_or(rest.len());
        let unit: u64 = match &rest[..length] {
            b"ns" => 1,
            b"us" | b"\xc2\xb5s" | b"\xce\xbcs" => 1_000,
            b"ms" => 1_000_000,
            b"s" => 1_000_000_000,
            b"m" => 60_000_000_000,
            b"h" => 3_600_000_000_000,
            _ => return None,
        };
        rest = &rest[length..];
        if whole > LIMIT / unit {
            return None;
        }
        whole *= unit;
        if fraction > 0 {
            whole += (fraction as f64 * (unit as f64 / scale)) as u64;
            if whole > LIMIT {
                return None;
            }
        }
        total += whole;
        if total > LIMIT {
            return None;
        }
    }
    if negative {
        return Some((total as i64).wrapping_neg());
    }
    i64::try_from(total).ok()
}
/// Duration text such as `1h30m0s`, the inverse of [`parse_duration`].
pub fn duration_text(nanoseconds: i64) -> String {
    let mut value = nanoseconds.unsigned_abs();
    let sign = if nanoseconds < 0 { "-" } else { "" };
    // Trailing zeros of the fraction are dropped, and the point with them.
    fn fraction(value: &mut u64, precision: usize) -> String {
        let mut digits = Vec::new();
        let mut print = false;
        for _ in 0..precision {
            let digit = (*value % 10) as u8;
            print |= digit != 0;
            if print {
                digits.push(b'0' + digit);
            }
            *value /= 10;
        }
        if print {
            digits.push(b'.');
        }
        digits.reverse();
        String::from_utf8(digits).unwrap()
    }
    if value == 0 {
        return "0s".into();
    }
    if value < 1_000_000_000 {
        let (precision, unit) = match value {
            ..1_000 => (0, "ns"),
            1_000..1_000_000 => (3, "\u{b5}s"),
            _ => (6, "ms"),
        };
        let tail = fraction(&mut value, precision);
        return format!("{sign}{value}{tail}{unit}");
    }
    let tail = fraction(&mut value, 9);
    let mut text = format!("{}{tail}s", value % 60);
    value /= 60;
    if value > 0 {
        text = format!("{}m{text}", value % 60);
        value /= 60;
        if value > 0 {
            text = format!("{value}h{text}");
        }
    }
    format!("{sign}{text}")
}
/// A double-quoted string with escapes, for diagnostics.
pub fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\u{7}' => quoted.push_str("\\a"),
            '\u{8}' => quoted.push_str("\\b"),
            '\u{c}' => quoted.push_str("\\f"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            '\u{b}' => quoted.push_str("\\v"),
            other if (other as u32) < 0x20 || other as u32 == 0x7f => {
                quoted.push_str(&format!("\\x{:02x}", other as u32))
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> FlagSet {
        let mut flags = FlagSet::new("tool").quiet();
        flags.string("project", "", "explicit project");
        flags.bool("offline", false, "no network");
        flags.int("count", 3, "a `number` of things");
        flags
    }
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }
    #[test]
    fn flag_forms() {
        let mut flags = set();
        flags
            .parse(args(&[
                "-project",
                "/a",
                "--offline",
                "-count=0x10",
                "rest",
                "-x",
            ]))
            .unwrap();
        assert_eq!(flags.get_string("project"), "/a");
        assert!(flags.get_bool("offline") && flags.is_set("offline"));
        assert_eq!(flags.get_int("count"), 16);
        assert_eq!(flags.args(), args(&["rest", "-x"]));
        let mut flags = set();
        flags
            .parse(args(&["--project=/b", "-offline=false", "--", "-count"]))
            .unwrap();
        assert_eq!(flags.get_string("project"), "/b");
        assert!(!flags.get_bool("offline") && !flags.is_set("count"));
        assert_eq!(flags.args(), args(&["-count"]));
        // A non-boolean flag consumes the next argument even when it looks like a flag.
        let mut flags = set();
        flags.parse(args(&["-project", "-offline"])).unwrap();
        assert_eq!(flags.get_string("project"), "-offline");
        assert!(!flags.get_bool("offline"));
    }
    #[test]
    fn flag_errors() {
        for (input, message) in [
            (vec!["-missing"], "flag provided but not defined: -missing"),
            (vec!["-project"], "flag needs an argument: -project"),
            (vec!["---x"], "bad flag syntax: ---x"),
            (vec!["-=x"], "bad flag syntax: -=x"),
            (
                vec!["-offline=maybe"],
                "invalid boolean value \"maybe\" for -offline: parse error",
            ),
            (
                vec!["-count", "x"],
                "invalid value \"x\" for flag -count: parse error",
            ),
            (
                vec!["-count", "99999999999999999999"],
                "invalid value \"99999999999999999999\" for flag -count: value out of range",
            ),
        ] {
            assert_eq!(
                set().parse(args(&input)),
                Err(FlagError::Invalid(message.into()))
            );
        }
        assert_eq!(set().parse(args(&["-h"])), Err(FlagError::Help));
        assert_eq!(set().parse(args(&["--help"])), Err(FlagError::Help));
    }
    #[test]
    fn usage_text() {
        assert_eq!(
            set().usage(),
            "Usage of tool:\n  -count number\n    \ta number of things (default 3)\n  -offline\n    \tno network\n  -project string\n    \texplicit project\n"
        );
    }
}
