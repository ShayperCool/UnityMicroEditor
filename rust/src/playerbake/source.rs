//! Typed reads of authored Unity YAML values.
use super::unsupported;
use anyhow::{Result, bail};
use serde_json::{Map, Value};

static NULL: Value = Value::Null;

pub(crate) fn get<'a>(map: &'a Map<String, Value>, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&NULL)
}

pub(crate) fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>> {
    match value {
        Value::Object(map) => Ok(map),
        _ => bail!("{what} is not a mapping"),
    }
}

/// An absent or empty sequence reads as no items.
pub(crate) fn list<'a>(value: &'a Value, what: &str) -> Result<&'a [Value]> {
    match value {
        Value::Array(items) => Ok(items),
        Value::Null => Ok(&[]),
        Value::String(text) if text.is_empty() => Ok(&[]),
        _ => bail!("{what} is not a sequence"),
    }
}

/// The scalar as Unity wrote it.
pub(crate) fn text(value: &Value) -> Result<String> {
    Ok(match value {
        Value::Null => String::new(),
        Value::Bool(flag) => (*flag as u8).to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => bail!("expected a scalar"),
    })
}

pub(crate) fn float(value: &Value, what: &str) -> Result<f32> {
    let text = text(value)?;
    Ok(match text.as_str() {
        "" => 0.0,
        "Infinity" | "inf" | "+Infinity" => f32::INFINITY,
        "-Infinity" | "-inf" => f32::NEG_INFINITY,
        "NaN" | "nan" => return Err(unsupported(format!("{what} is not a number"))),
        // Parsed directly to single precision, as Unity reads it.
        other => match other.parse::<f32>() {
            Ok(number) => number,
            Err(_) => bail!("{what} is not a float: {other:?}"),
        },
    })
}

pub(crate) fn int(value: &Value, what: &str) -> Result<i64> {
    let text = text(value)?;
    if text.is_empty() {
        return Ok(0);
    }
    match text.parse::<i64>() {
        Ok(number) => Ok(number),
        Err(_) => bail!("{what} is not an integer: {text:?}"),
    }
}

pub(crate) fn flag(value: &Value, what: &str) -> Result<bool> {
    Ok(int(value, what)? != 0)
}

/// Refuses a source in which the YAML reader would drop text the engine
/// keeps. The engine reads a `#` after a blank as part of a plain value, and
/// the Editor writes such a value without quotes; to the reader used here the
/// `#` starts a comment.
pub(crate) fn refuse_comment_marks(yaml: &[u8]) -> Result<()> {
    let mut previous = b'\n';
    for &byte in yaml {
        if byte == b'#' && matches!(previous, b' ' | b'\t' | b'\n') {
            return Err(unsupported(
                "authored text with a `#` that YAML reads as the start of a comment",
            ));
        }
        previous = byte;
    }
    Ok(())
}

/// Whether an object reference is `{fileID: 0}` or absent.
pub(crate) fn null_reference(value: &Value) -> Result<bool> {
    match value {
        Value::Null => Ok(true),
        Value::Object(map) => Ok(text(get(map, "fileID"))? == "0" || map.is_empty()),
        _ => bail!("expected an object reference"),
    }
}
