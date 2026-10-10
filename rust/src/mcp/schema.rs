//! Tool input schemas rendered from a field list: one property per field in
//! declaration order, `required` for the fields that are not optional, closed
//! objects, and the field's text as description.
use crate::jsondoc::quote;

fn typed(kind: &str, description: &str) -> String {
    if description.is_empty() {
        format!("{{\"type\":{kind}}}")
    } else {
        format!("{{\"type\":{kind},\"description\":{}}}", quote(description))
    }
}
/// A string.
pub(crate) fn string(description: &str) -> String {
    typed("\"string\"", description)
}
/// An integer.
pub(crate) fn integer(description: &str) -> String {
    typed("\"integer\"", description)
}
/// A boolean.
pub(crate) fn boolean() -> String {
    typed("\"boolean\"", "")
}
/// A boolean that may be `null`.
pub(crate) fn optional_boolean(description: &str) -> String {
    typed("[\"null\",\"boolean\"]", description)
}
/// An array (or `null`) whose items follow the schema `items`.
pub(crate) fn array(items: &str, description: &str) -> String {
    let mut text = format!("{{\"type\":[\"null\",\"array\"],\"items\":{items}");
    if !description.is_empty() {
        text.push_str(&format!(",\"description\":{}", quote(description)));
    }
    text.push('}');
    text
}
/// An object with arbitrary keys; `values` is the schema of every value.
pub(crate) fn map(values: &str, description: &str) -> String {
    let mut text = String::from("{\"type\":\"object\"");
    if !description.is_empty() {
        text.push_str(&format!(",\"description\":{}", quote(description)));
    }
    text.push_str(&format!(",\"additionalProperties\":{values}}}"));
    text
}
/// Any JSON value, including raw JSON.
pub(crate) const ANY: &str = "true";

/// A struct: `(name, schema, required)` per field.
pub(crate) fn object(fields: &[(&str, String, bool)]) -> String {
    let mut text = String::from("{\"type\":\"object\"");
    if !fields.is_empty() {
        let properties: Vec<_> = fields
            .iter()
            .map(|(name, schema, _)| format!("{}:{schema}", quote(name)))
            .collect();
        text.push_str(&format!(",\"properties\":{{{}}}", properties.join(",")));
    }
    let required: Vec<_> = fields
        .iter()
        .filter(|(_, _, required)| *required)
        .map(|(name, _, _)| quote(name))
        .collect();
    if !required.is_empty() {
        text.push_str(&format!(",\"required\":[{}]", required.join(",")));
    }
    text.push_str(",\"additionalProperties\":false}");
    text
}

/// The schema of a tool reply.
pub(crate) fn reply() -> String {
    let error = "{\"type\":[\"null\",\"object\"],\"properties\":{\"code\":{\"type\":\"string\"},\"message\":{\"type\":\"string\"},\"details\":{\"type\":\"object\",\"additionalProperties\":true}},\"required\":[\"code\",\"message\"],\"additionalProperties\":false}";
    object(&[
        ("success", boolean(), true),
        ("status", string(""), false),
        ("data", ANY.into(), false),
        ("error", error.into(), false),
    ])
}
