//! `parity-compare` checks shared Editor/Player snapshots without discarding game fields.
use crate::flags::FlagSet;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashSet};

const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
/// Comparison stops descending once this many differences are recorded.
const MAX_DIFFERENCES: usize = 100;
/// The loader's own resource owner: ignored only if both its reserved name and type match.
const RESOURCE_OWNER_PATH: &str = "Raw scene imported assets";
const RESOURCE_OWNER_TYPE: &str = "UnityMicroEditor.Raw.RawSceneResources";
const NORMALIZATIONS: [&str; 4] = [
    "object/component source identities",
    "Editor globalObjectId and prefabInstance identity markers",
    "reserved RawSceneResources owner",
    "quaternion q and -q represent the same rotation",
];

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Difference {
    path: String,
    reference: Value,
    actual: Value,
    reason: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report<'a> {
    schema_version: u32,
    success: bool,
    reference: &'a str,
    actual: &'a str,
    reference_objects: usize,
    actual_objects: usize,
    absolute_tolerance: f64,
    normalizations: [&'static str; 4],
    differences: Vec<Difference>,
}

/// Why an envelope yields no snapshot.
#[derive(Debug)]
struct Unusable {
    message: String,
    /// The envelope itself reports a failure or an incomplete hierarchy, so no sibling
    /// field may be used in its place.
    conclusive: bool,
}

fn conclusive(message: &str) -> Unusable {
    Unusable {
        message: message.into(),
        conclusive: true,
    }
}

fn inconclusive(message: String) -> Unusable {
    Unusable {
        message,
        conclusive: false,
    }
}

/// Finds the complete object list in a snapshot file, an MCP reply or a runtime response.
fn objects(value: &Value) -> Result<Vec<Value>, Unusable> {
    let Value::Object(data) = value else {
        return Err(inconclusive("snapshot envelope is not an object".into()));
    };
    if data.get("success") == Some(&Value::Bool(false)) {
        return Err(conclusive("snapshot operation reported success=false"));
    }
    if data.get("isError") == Some(&Value::Bool(true)) {
        return Err(conclusive("MCP tool reported isError=true"));
    }
    if let Some(error) = data.get("error").filter(|error| !error.is_null()) {
        return Err(inconclusive(format!(
            "snapshot envelope contains error: {error}"
        )));
    }
    if let Some(Value::Array(list)) = data.get("objects") {
        if data.get("nextOffset").is_some_and(|next| !next.is_null()) {
            return Err(conclusive(
                "hierarchy snapshot is paginated; collect all pages before comparing",
            ));
        }
        if let Some(Value::Number(total)) = data.get("total")
            && total.as_f64() != Some(list.len() as f64)
        {
            return Err(conclusive(
                "hierarchy snapshot does not contain its declared total",
            ));
        }
        return Ok(list.clone());
    }
    for key in ["result", "structuredContent", "data"] {
        if let Some(nested) = data.get(key) {
            match objects(nested) {
                Ok(list) => return Ok(list),
                Err(error) if error.conclusive => return Err(error),
                Err(_) => {}
            }
        }
    }
    for key in ["content", "contents"] {
        let Some(Value::Array(blocks)) = data.get(key) else {
            continue;
        };
        for block in blocks {
            if let Some(Value::String(text)) = block.get("text")
                && let Ok(nested) = serde_json::from_str::<Value>(text)
                && let Ok(list) = objects(&nested)
            {
                return Ok(list);
            }
        }
    }
    Err(inconclusive(
        "no complete shared objects snapshot found".into(),
    ))
}

fn has_component(components: &[Value], name: &str) -> bool {
    components
        .iter()
        .any(|component| component.get("type").and_then(Value::as_str) == Some(name))
}

/// Drops the identities that legitimately differ between the Editor and the Player and
/// refuses snapshots whose objects cannot be paired by path.
fn normalize(list: Vec<Value>) -> Result<Vec<Value>, String> {
    let mut result = Vec::with_capacity(list.len());
    let mut seen = HashSet::new();
    for item in list {
        let Value::Object(mut object) = item else {
            return Err("snapshot object is not an object".into());
        };
        let path = match object.get("path") {
            Some(Value::String(path)) if !path.is_empty() => path.clone(),
            _ => return Err("snapshot object has no path".into()),
        };
        if !seen.insert(path.clone()) {
            return Err(format!("ambiguous duplicate object path {path}"));
        }
        let Some(Value::Array(mut components)) = object.remove("components") else {
            return Err(format!("object {path} has no component list"));
        };
        if path == RESOURCE_OWNER_PATH && has_component(&components, RESOURCE_OWNER_TYPE) {
            continue;
        }
        for key in ["source", "globalObjectId", "prefabInstance"] {
            object.remove(key);
        }
        for (index, component) in components.iter_mut().enumerate() {
            let Value::Object(component) = component else {
                return Err(format!("object {path} component {index} is invalid"));
            };
            component.remove("source");
        }
        object.insert("components".into(), Value::Array(components));
        result.push(Value::Object(object));
    }
    Ok(result)
}

/// A quaternion and its negation are the same rotation.
fn quaternion_equal(
    reference: &Map<String, Value>,
    actual: &Map<String, Value>,
    tolerance: f64,
) -> bool {
    if reference.len() != 4 || actual.len() != 4 {
        return false;
    }
    let number = |map: &Map<String, Value>, key: &str| match map.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        _ => None,
    };
    [1.0, -1.0].into_iter().any(|sign| {
        ["x", "y", "z", "w"].into_iter().all(|key| {
            match (number(reference, key), number(actual, key)) {
                (Some(a), Some(b)) => (a - sign * b).abs() <= tolerance,
                _ => false,
            }
        })
    })
}

fn compare(
    reference: &Value,
    actual: &Value,
    path: &str,
    tolerance: f64,
    differences: &mut Vec<Difference>,
) {
    if differences.len() >= MAX_DIFFERENCES {
        return;
    }
    let mut add = |reason| {
        differences.push(Difference {
            path: path.into(),
            reference: reference.clone(),
            actual: actual.clone(),
            reason,
        })
    };
    match (reference, actual) {
        (Value::Object(reference), Value::Object(actual)) => {
            if (path.ends_with(".rotation") || path.ends_with(".localRotation"))
                && quaternion_equal(reference, actual, tolerance)
            {
                return;
            }
            let keys: BTreeSet<&String> = reference.keys().chain(actual.keys()).collect();
            for key in keys {
                let field = format!("{path}.{key}");
                match (reference.get(key), actual.get(key)) {
                    (Some(a), Some(b)) => compare(a, b, &field, tolerance, differences),
                    (a, b) => differences.push(Difference {
                        path: field,
                        reference: a.cloned().unwrap_or(Value::Null),
                        actual: b.cloned().unwrap_or(Value::Null),
                        reason: "field missing",
                    }),
                }
            }
        }
        (Value::Array(reference), Value::Array(actual)) => {
            if reference.len() != actual.len() {
                add("array length mismatch");
            }
            for (index, (a, b)) in reference.iter().zip(actual).enumerate() {
                compare(a, b, &format!("{path}[{index}]"), tolerance, differences);
            }
        }
        (Value::Object(_) | Value::Array(_), _) => add("type mismatch"),
        (Value::Number(reference), actual) => {
            // `as_f64` is `None` for anything but a finite number.
            let equal = matches!(
                (reference.as_f64(), actual.as_f64()),
                (Some(a), Some(b)) if (a - b).abs() <= tolerance
            );
            if !equal {
                add("numeric mismatch");
            }
        }
        _ => {
            if reference != actual {
                add("value mismatch");
            }
        }
    }
}

fn read_snapshot(path: &str) -> Result<Vec<Value>, String> {
    let data = std::fs::read(path).map_err(|error| format!("{path}: {error}"))?;
    if data.len() > MAX_SNAPSHOT_BYTES {
        return Err("snapshot exceeds 16 MiB".into());
    }
    let value: Value = serde_json::from_slice(&data).map_err(|error| error.to_string())?;
    normalize(objects(&value).map_err(|error| error.message)?)
}

/// Whether the snapshots are equal; an error means the evidence cannot be compared.
fn run(flags: &FlagSet) -> Result<bool, String> {
    let (reference_path, actual_path) = (flags.get_string("reference"), flags.get_string("actual"));
    let tolerance = flags.get_float("tolerance");
    if reference_path.is_empty()
        || actual_path.is_empty()
        || !tolerance.is_finite()
        || tolerance < 0.0
    {
        return Err("provide reference, actual and a finite nonnegative tolerance".into());
    }
    let reference =
        read_snapshot(&reference_path).map_err(|error| format!("reference: {error}"))?;
    let actual = read_snapshot(&actual_path).map_err(|error| format!("actual: {error}"))?;
    let (reference_objects, actual_objects) = (reference.len(), actual.len());
    let mut differences = Vec::new();
    compare(
        &Value::Array(reference),
        &Value::Array(actual),
        "objects",
        tolerance,
        &mut differences,
    );
    let success = differences.is_empty();
    let report = Report {
        schema_version: 1,
        success,
        reference: &reference_path,
        actual: &actual_path,
        reference_objects,
        actual_objects,
        absolute_tolerance: tolerance,
        normalizations: NORMALIZATIONS,
        differences,
    };
    super::emit(&flags.get_string("output"), &super::pretty(&report)?)
        .map_err(|error| error.to_string())?;
    Ok(success)
}

/// Exit status 0 for equal snapshots, 1 for a behavioral difference, 2 for invalid or
/// incomplete evidence.
pub fn main(args: Vec<String>) -> i32 {
    let mut flags = FlagSet::new("unity-micro-editor parity-compare");
    flags.string("reference", "", "Editor shared snapshot JSON");
    flags.string(
        "actual",
        "",
        "Player shared snapshot JSON or MCP/runtime response",
    );
    flags.string("output", "", "optional comparison report JSON");
    flags.float("tolerance", 1e-5, "absolute floating-point tolerance");
    if let Err(status) = super::parse(&mut flags, args) {
        return status;
    }
    match run(&flags) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn differences(reference: &Value, actual: &Value, path: &str) -> Vec<Difference> {
        let mut differences = Vec::new();
        compare(reference, actual, path, 1e-5, &mut differences);
        differences
    }

    #[test]
    fn normalize_preserves_serialized_source_and_unsupported_components() {
        let Value::Array(value) = decode(
            r#"[{"path":"Root","source":{"fileId":"1"},"globalObjectId":"abc","prefabInstance":true,"components":[{"type":"Game.Probe","source":{"fileId":"2"},"fields":{"serialized":{"source":42}}},{"type":"MissingScript"}]}]"#,
        ) else {
            unreachable!()
        };
        let list = normalize(value).unwrap();
        let object = &list[0];
        for identity in ["source", "globalObjectId", "prefabInstance"] {
            assert!(object.get(identity).is_none(), "{identity} retained");
        }
        let components = object["components"].as_array().unwrap();
        assert_eq!(components.len(), 2, "unsupported component was discarded");
        assert!(components[0].get("source").is_none());
        assert_eq!(components[0]["fields"]["serialized"]["source"], json!(42));
    }

    #[test]
    fn normalize_ignores_only_the_typed_resource_owner() {
        let owner =
            json!({"path": RESOURCE_OWNER_PATH, "components": [{"type": RESOURCE_OWNER_TYPE}]});
        let namesake = json!({"path": RESOURCE_OWNER_PATH, "components": [{"type": "Transform"}]});
        assert!(normalize(vec![owner]).unwrap().is_empty());
        assert_eq!(normalize(vec![namesake]).unwrap().len(), 1);
    }

    #[test]
    fn comparison_checks_lifecycle_and_component_order() {
        let reference = decode(
            r#"{"components":[{"type":"Transform"},{"type":"Probe","fields":{"awakeCount":1}}]}"#,
        );
        for text in [
            r#"{"components":[{"type":"Probe","fields":{"awakeCount":1}},{"type":"Transform"}]}"#,
            r#"{"components":[{"type":"Transform"},{"type":"Probe","fields":{"awakeCount":2}}]}"#,
        ] {
            assert!(
                !differences(&reference, &decode(text), "objects[0]").is_empty(),
                "behavior difference accepted: {text}"
            );
        }
    }

    #[test]
    fn quaternion_sign_equivalence_and_absolute_tolerance() {
        let a =
            decode(r#"{"rotation":{"x":0,"y":0.5,"z":0,"w":0.8660254},"position":{"x":100000}}"#);
        let b = decode(
            r#"{"rotation":{"x":0,"y":-0.5,"z":0,"w":-0.8660254},"position":{"x":100000.000001}}"#,
        );
        assert_eq!(differences(&a, &b, "object"), []);
        let c = decode(
            r#"{"rotation":{"x":0,"y":-0.5,"z":0,"w":-0.8660254},"position":{"x":100000.01}}"#,
        );
        let found = differences(&a, &c, "object");
        assert_eq!(found.len(), 1, "relative tolerance hid position difference");
        assert_eq!(found[0].path, "object.position.x");
        assert_eq!(found[0].reason, "numeric mismatch");
    }

    #[test]
    fn missing_fields_and_type_changes_are_differences() {
        let found = differences(
            &json!({"a": 1, "b": "text", "c": [1, 2]}),
            &json!({"b": 2, "c": [1], "d": null}),
            "object",
        );
        let reasons: Vec<_> = found
            .iter()
            .map(|difference| (difference.path.as_str(), difference.reason))
            .collect();
        assert_eq!(
            reasons,
            [
                ("object.a", "field missing"),
                ("object.b", "value mismatch"),
                ("object.c", "array length mismatch"),
                ("object.d", "field missing"),
            ]
        );
    }

    #[test]
    fn rejects_errors_partial_and_duplicate_snapshots() {
        for text in [
            r#"{"success":false,"objects":[]}"#,
            r#"{"isError":true,"structuredContent":{"objects":[]}}"#,
            r#"{"result":{"objects":[],"total":2}}"#,
            r#"{"objects":[],"nextOffset":50}"#,
        ] {
            assert!(objects(&decode(text)).is_err(), "accepted {text}");
        }
        let Value::Array(duplicate) =
            decode(r#"[{"path":"A","components":[]},{"path":"A","components":[]}]"#)
        else {
            unreachable!()
        };
        assert!(normalize(duplicate).is_err(), "duplicate paths accepted");
    }

    #[test]
    fn finds_snapshots_inside_replies() {
        let snapshot = r#"{"objects":[{"path":"A","components":[]}],"total":1}"#;
        let wrapped = json!({"result": {"content": [{"type": "text", "text": snapshot}]}});
        for value in [
            decode(snapshot),
            json!({"success": true, "data": decode(snapshot)}),
            wrapped,
        ] {
            assert_eq!(objects(&value).unwrap().len(), 1);
        }
    }
}
