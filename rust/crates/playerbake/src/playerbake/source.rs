//! Typed reads of authored Unity YAML values.
use anyhow::{Result, bail};
use serde_json::Value;

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
