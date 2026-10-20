//! Deciding whether a new revision changes anything a Player loads.
use super::read_file;
use crate::{
    project::{Map, Value, lexpath::rune_lower, value},
    sysutil,
};
use anyhow::{Result, bail};

/// Metadata-only writes must not reset a running scene. Source hashes remain
/// validated by Prepare; the Player only needs a reload for changed runtime data.
pub fn scene_runtime_equal(previous: &str, next: &str) -> Result<bool> {
    fn decode(path: &str) -> Result<Map> {
        let mut document = match parse_document(&read_file(path)?)? {
            Value::Object(document) => document,
            Value::Null => Map::new(),
            other => bail!(
                "json: cannot unmarshal {} into value of type object",
                match other {
                    Value::Bool(_) => "bool",
                    Value::String(_) => "string",
                    Value::Array(_) => "array",
                    _ => "number",
                }
            ),
        };
        for key in ["revision", "manifest", "contentHash", "diagnostics"] {
            document.remove(key);
        }
        if let Some(dependencies) = document.get_mut("sourceDependencies") {
            normalize_scene_dependencies(dependencies);
        }
        Ok(document)
    }
    // Number literals compare verbatim here.
    Ok(decode(previous)? == decode(next)?)
}

/// An authored scene or prefab changes the scene the worker writes, whose own
/// hash the manifest holds; its source hash alone changes nothing.
pub fn normalize_scene_dependencies(value: &mut Value) {
    let Value::Array(dependencies) = value else {
        return;
    };
    for item in dependencies {
        let Value::Object(dependency) = item else {
            continue;
        };
        let path = dependency.get("path").and_then(Value::as_str).unwrap_or("");
        if matches!(
            rune_lower(sysutil::ext(path)).as_str(),
            ".unity" | ".prefab"
        ) {
            dependency.remove("sha256");
            dependency.remove("bytes");
        }
    }
}

/// Decodes a whole document, keeping number literals verbatim.
fn parse_document(data: &[u8]) -> Result<Value> {
    let (value, used) = value::parse_json(data, true)?;
    match data[used..]
        .iter()
        .find(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        None => Ok(value),
        Some(&byte) => bail!(
            "invalid character {:?} after top-level value",
            char::from(byte)
        ),
    }
}
