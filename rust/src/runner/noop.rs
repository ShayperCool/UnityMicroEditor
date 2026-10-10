//! Deciding whether a new revision changes anything a Player loads.
use super::{patch::parse_document, read_file};
use crate::{
    project::{Map, Value, lexpath::rune_lower},
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

/// Authored scenes and prefabs are fully represented in the runtime graph. Other
/// dependencies can hold file-backed content that a scene patch cannot reload.
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
