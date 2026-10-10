//! Tests of the raw scene preparation session.
use super::{
    Options, Session,
    value::{self, Map, Value},
};
use std::{fs, path::Path};

mod discovery;
mod imported;
mod native;
mod session;
mod sources;

pub(crate) const FIXTURE_GUID: &str = "11111111111111111111111111111111";
pub(crate) const FIXTURE_SCENE: &str = "%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &10
GameObject:
  m_Component:
  - component: {fileID: 11}
  m_Name: Original
  m_IsActive: 1
--- !u!4 &11
Transform:
  m_GameObject: {fileID: 10}
  m_LocalPosition: {x: 0, y: 0, z: 0}
  m_LocalRotation: {x: 0, y: 0, z: 0, w: 1}
  m_LocalScale: {x: 1, y: 1, z: 1}
  m_Father: {fileID: 0}
  m_Children: []
--- !u!1660057539 &9223372036854775807
SceneRoots:
  m_Roots:
  - {fileID: 11}
";

pub(crate) fn write_fixture_file(path: impl AsRef<Path>, content: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A temporary project; the directory lives as long as this value.
pub(crate) struct Fixture {
    pub session: Session,
    pub project: String,
    pub scene: String,
    pub output: String,
    _guard: tempfile::TempDir,
}

/// One scene, a schema 1 baseline and no catalog.
pub(crate) fn fixture_session() -> Fixture {
    fixture_with(serde_json::json!({}), None)
}

/// A fixture whose baseline is extended by `extra` and which may carry a
/// native catalog (`project` and engine are filled in when absent).
pub(crate) fn fixture_with(
    extra: serde_json::Value,
    catalog: Option<serde_json::Value>,
) -> Fixture {
    let guard = tempfile::tempdir().unwrap();
    let project = guard.path().canonicalize().unwrap();
    let project = project.to_str().unwrap().to_string();
    let scene = format!("{project}/Assets/Scenes/Sample.unity");
    write_fixture_file(&scene, FIXTURE_SCENE);
    write_fixture_file(format!("{scene}.meta"), format!("guid: {FIXTURE_GUID}\n"));
    let mut manifest = serde_json::json!({"schemaVersion":1,"supported":true,"unityVersion":"6000.6.3f1","projectRoot":project,"scenePath":"Assets/Scenes/Sample.unity","sceneName":"Sample","objects":[],"assets":[],"assemblies":["existing.dll"],"sourceDependencies":[]});
    for (key, item) in extra.as_object().cloned().unwrap_or_default() {
        manifest[key] = item;
    }
    let path = format!("{project}/baseline.json");
    fs::write(&path, manifest.to_string()).unwrap();
    let mut native_catalog = String::new();
    if let Some(mut catalog) = catalog {
        for (key, item) in [
            ("schemaVersion", serde_json::json!(1)),
            ("engineVersion", serde_json::json!("6000.6.3f1")),
            ("project", serde_json::json!(project)),
        ] {
            if catalog.get(key).is_none() {
                catalog[key] = item;
            }
        }
        native_catalog = format!("{project}/catalog.json");
        fs::write(&native_catalog, catalog.to_string()).unwrap();
    }
    let session = Session::new(Options {
        project: project.clone(),
        manifest: path,
        native_catalog,
        ..Options::default()
    })
    .unwrap();
    Fixture {
        session,
        output: format!("{project}/output"),
        project,
        scene,
        _guard: guard,
    }
}

/// `{err:#}`: the full `a: b` chain of a wrapped error.
pub(crate) fn message(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

/// The decoded `dataJson` of a manifest object.
pub(crate) fn data_json(object: &Value) -> Map {
    match value::parse_use_number(&value::str(object.field("dataJson"))).unwrap() {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Converts literal JSON (number literals kept verbatim) into a session value.
pub(crate) fn json(value: serde_json::Value) -> Value {
    value::parse_use_number(&value.to_string()).unwrap()
}
