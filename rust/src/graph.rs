//! Authored graph diagnostics over `serde_json` values. Expansion and override
//! semantics live in `crate::project`; this module adapts them to the document
//! type produced by `crate::yaml`, where floats keep a fraction or exponent.
use crate::{
    cache::{CacheStats, SourceCache},
    project::{self, Session, value},
    yaml::Document,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Map, Number, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub use crate::project::AuthoredIdentity;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthoredGraph {
    pub path: String,
    pub guid: String,
    pub objects: Vec<GraphObject>,
    pub local_ids: BTreeMap<String, String>,
    pub dependencies: Vec<Dependency>,
    pub ignored_prefab_override_diagnostics: Vec<Value>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphObject {
    #[serde(flatten)]
    pub document: Document,
    pub source_path: String,
    pub guid: String,
    pub identities: Vec<AuthoredIdentity>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Dependency {
    pub path: String,
    pub sha256: String,
}

/// A catalog-less authored project with its caches kept between calls.
pub struct SourceProject {
    pub project: PathBuf,
    /// `stats` holds the counters of the most recent `authored_graph` call.
    pub cache: SourceCache,
    session: Session,
}

fn to_serde(value: &value::Value) -> Result<Value> {
    Ok(match value {
        value::Value::Null => Value::Null,
        value::Value::Bool(b) => Value::Bool(*b),
        value::Value::String(text) => Value::String(text.clone()),
        value::Value::Int(n) => Value::from(*n),
        value::Value::Int32(n) => Value::from(*n),
        value::Value::Uint(n) => Value::from(*n),
        // JSON cannot represent a non-finite override result.
        value::Value::Float(f) => Value::Number(
            Number::from_f64(*f)
                .with_context(|| format!("json: unsupported value: {}", value::float_layout(*f)))?,
        ),
        value::Value::Number(text) => serde_json::from_str(text)?,
        value::Value::Raw(text) => serde_json::from_str(text)?,
        value::Value::Array(items) => {
            Value::Array(items.iter().map(to_serde).collect::<Result<_>>()?)
        }
        value::Value::Object(map) => Value::Object(to_serde_map(map)?),
    })
}
fn to_serde_map(map: &value::Map) -> Result<Map<String, Value>> {
    map.iter()
        .map(|(key, item)| Ok((key.clone(), to_serde(item)?)))
        .collect()
}
fn from_serde_map(map: &Map<String, Value>) -> value::Map {
    map.iter()
        .map(|(key, item)| (key.clone(), value::Value::from_yaml(item)))
        .collect()
}

impl SourceProject {
    pub fn new(project: &Path) -> Result<Self> {
        ensure!(
            !project.as_os_str().is_empty(),
            "an explicit project is required"
        );
        let session = Session::authored_only(project.to_str().context("project is not UTF-8")?)?;
        Ok(Self {
            project: PathBuf::from(session.project()),
            cache: SourceCache::default(),
            session,
        })
    }
    /// Watcher events: metadata and directory changes rebuild the GUID index;
    /// cached files are revalidated by stat on every read.
    pub fn invalidate(&mut self, paths: &[String]) {
        self.session.invalidate(paths);
    }
    pub fn authored_graph(&mut self, logical: &str) -> Result<AuthoredGraph> {
        let graph = {
            let mut inner = self.session.lock();
            inner.cache.stats = CacheStats::default();
            let graph = inner.authored_graph(logical);
            self.cache.stats = inner.cache.stats;
            graph?
        };
        Ok(AuthoredGraph {
            path: graph.path,
            guid: graph.guid,
            objects: graph
                .objects
                .into_iter()
                .map(|object| {
                    Ok(GraphObject {
                        document: Document {
                            class_id: object.document.class_id,
                            data: to_serde_map(object.document.fields())?,
                            file_id: object.document.file_id,
                            type_name: object.document.type_name,
                            stripped: object.document.stripped,
                        },
                        source_path: object.source_path,
                        guid: object.guid,
                        identities: object.identities,
                    })
                })
                .collect::<Result<_>>()?,
            local_ids: graph.local_ids,
            dependencies: graph
                .dependencies
                .into_iter()
                .map(|d| Dependency {
                    path: d.path,
                    sha256: d.sha256,
                })
                .collect(),
            ignored_prefab_override_diagnostics: graph
                .ignored_prefab_override_diagnostics
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()?,
        })
    }
}

/// Applies one saved override with the runtime expander's typed semantics.
/// Parents created for an indexed path remain when the final assignment is refused.
pub fn apply_override(
    data: &mut Map<String, Value>,
    path: &str,
    raw: &Value,
    reference: &Value,
) -> Result<()> {
    let mut fields = from_serde_map(data);
    let reference = reference
        .as_object()
        .map(from_serde_map)
        .unwrap_or_default();
    let result = project::apply_authored_override(
        &mut fields,
        path,
        &value::Value::from_yaml(raw),
        &reference,
    );
    *data = to_serde_map(&fields)?;
    result
}
