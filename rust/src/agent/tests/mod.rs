mod actions_tests;
mod bootstrap_tests;
mod closure_tests;
mod configuration_tests;
mod content_tests;
mod defaults_tests;
mod host_tests;
mod jobs_tests;
mod recovery_tests;

use super::*;
use crate::{mcp::HandlerValue, sysutil::testing::temp_dir};
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use tempfile::TempDir;

pub(super) fn ctx() -> Context {
    Context::background()
}
pub(super) fn write(path: &str, content: impl AsRef<[u8]>) {
    fs::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}
pub(super) fn write_mode(path: &str, content: impl AsRef<[u8]>, mode: u32) {
    write(path, content);
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
pub(super) fn absent(path: &str) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}
/// The decoded JSON of a handler result.
pub(super) fn decoded(value: &HandlerValue) -> Value {
    serde_json::from_slice(reply_bytes(value)).unwrap()
}
pub(super) fn is_map(value: &HandlerValue) -> bool {
    matches!(value, HandlerValue::Map(_))
}
/// The code of a typed `AuthoringError`, empty for any other error.
pub(super) fn code<T>(result: &Result<T>) -> String {
    match result {
        Err(error) => typed(error)
            .map(|detail| detail.code.clone())
            .unwrap_or_default(),
        Ok(_) => String::new(),
    }
}
pub(super) fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}
/// An untyped JSON object literal.
pub(super) fn any(value: Value) -> Any {
    value::parse_json(&bytes(&value), true).unwrap().0
}
pub(super) fn any_map(value: Value) -> AnyMap {
    match any(value) {
        Any::Object(map) => map,
        _ => panic!("not an object"),
    }
}
pub(super) fn immutable(path: &str, value: Value) {
    util::agent_immutable_json(path, &any(value)).unwrap();
}

pub(super) fn review_project() -> (TempDir, String) {
    let (guard, project) = temp_dir();
    for (path, content) in [
        (
            "ProjectSettings/ProjectVersion.txt",
            format!("m_EditorVersion: {UNITY_VERSION}\n"),
        ),
        (
            "Packages/manifest.json",
            "{\"dependencies\":{\"com.example.test\":\"1.2.3\"},\"scopedRegistries\":[{\"name\":\"example\",\"url\":\"https://example.invalid\"}],\"projectMarker\":{\"value\":7}}\n".to_string(),
        ),
        (
            "Assets/Sample.cs",
            "public class Sample { public int value = 1; }\n".to_string(),
        ),
        (
            "Assets/Sample.cs.meta",
            "fileFormatVersion: 2\nguid: 11111111111111111111111111111111\nMonoImporter:\n  externalObjects: {}\n  serializedVersion: 2\n".to_string(),
        ),
        (
            "Assets/Other.cs",
            "public class Other { public int value = 2; }\n".to_string(),
        ),
        (
            "Assets/Other.cs.meta",
            "fileFormatVersion: 2\nguid: 22222222222222222222222222222222\nMonoImporter:\n  externalObjects: {}\n  serializedVersion: 2\n".to_string(),
        ),
    ] {
        write_mode(&join(&[&project, path]), content, 0o600);
    }
    (guard, project)
}

pub(super) fn plain_service(project: &str) -> Service {
    Service::new(Options {
        project: project.into(),
        lifetime: Some(ctx()),
        ..Default::default()
    })
}
