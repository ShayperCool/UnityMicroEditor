//! The agent/project-actions layer. Everything written beneath a project's
//! `Library/UnityMicroEditor` (job receipts, preparation requests and results,
//! snapshots, catalogs, baselines) has fixed bytes, so durable work can be resumed.
//!
//! Untyped JSON values are `project::Value` here: it keeps the distinction between
//! number literals (kept verbatim) and f64 that the written files depend on.
mod actions;
mod artifacts;
mod bootstrap;
mod catalog;
mod cli;
mod configuration;
mod defaults;
mod defaults_closure;
mod host;
mod jobs;
mod native;
mod progress;
mod remote;
mod scene;
mod source_prepare;
mod source_scene;
mod staging;
#[cfg(test)]
mod tests;

pub use actions::{install_agent_package, project_build_arguments, validate_cached_preparation};
pub use artifacts::{
    PreparationArtifacts, capture_preparation_artifacts, validate_preparation_artifacts,
};
pub use catalog::freeze_source_catalog;
pub use cli::{
    prepare_agent_project, prepare_agent_project_with_native, run_auto_prepare_cli,
    run_build_project_cli, run_component_defaults_cli, run_mcp_cli, run_project_prepare_cli,
};
pub use configuration::{ConfigurationSelection, configuration_revision};
pub use defaults::{DefaultsCatalog, DefaultsProfile, component_defaults_profile};
pub use scene::{publish_agent_baseline, validate_scene_lighting};
pub use source_scene::{SourceSceneProof, build_source_scene_proof};
pub use staging::{
    SourceSnapshot, clone_staging, snapshot_source, sync_staging, validate_staged_metadata,
    verify_snapshot,
};

use crate::{
    agentcontrol::Controller,
    authoring::AuthoringError,
    cancel::{Canceled, Context},
    compiler::NilMap,
    editorbridge::Client,
    jsondoc,
    mcp::{HandlerError, HandlerValue},
    project::value::{self, Map as AnyMap, Value as Any},
    sysutil, util,
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Condvar, Mutex, RwLock},
};

pub(crate) const UNITY_VERSION: &str = crate::ENGINE_VERSION;

/// JSON `null` decodes to the default value.
pub(crate) fn nz<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    deserializer: D,
) -> std::result::Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
fn is_empty(value: &str) -> bool {
    value.is_empty()
}

/// The durable `result.json` of a preparation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preparation {
    #[serde(deserialize_with = "nz")]
    pub schema_version: i64,
    #[serde(deserialize_with = "nz")]
    pub request_id: String,
    #[serde(deserialize_with = "nz")]
    pub project: String,
    #[serde(deserialize_with = "nz")]
    pub scene: String,
    #[serde(deserialize_with = "nz")]
    pub baseline: String,
    #[serde(deserialize_with = "nz")]
    pub host_player: String,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub native_catalog: String,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub host_evidence: String,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub engine_profile: String,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub package_snapshot: String,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub artifact_receipt: String,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub component_defaults_profile: String,
    #[serde(deserialize_with = "nz", skip_serializing_if = "is_empty")]
    pub reference_graph: String,
    /// Where the host build left the Addressables content its build
    /// processors produced. Absent when the host build produced none.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub host_content: BTreeMap<String, String>,
    pub backends: NilMap<String>,
}

/// What constructs a `Service`.
#[derive(Clone, Default)]
pub struct Options {
    pub project: String,
    pub editor: String,
    pub host: String,
    pub scene: String,
    pub baseline: String,
    pub engine_profile_path: String,
    pub no_editor: bool,
    /// Project actions run as durable detached jobs.
    pub asynchronous: bool,
    pub controller: Option<Controller>,
    pub lifetime: Option<Context>,
}

#[derive(Default)]
pub(crate) struct Selection {
    pub scene: String,
    pub baseline: String,
}
#[derive(Default)]
pub(crate) struct Job {
    pub active: bool,
    pub receipt: Option<jobs::JobReceipt>,
}
#[derive(Default)]
pub(crate) struct Jobs {
    pub jobs: HashMap<String, Arc<Mutex<Job>>>,
    pub context: Option<Context>,
    pub closed: bool,
    pub auto_preparation_id: String,
}
pub(crate) struct Inner {
    pub project: String,
    pub editor: String,
    pub host: String,
    pub engine_profile_path: String,
    pub no_editor: bool,
    pub asynchronous: bool,
    pub controller: Option<Controller>,
    pub lifetime: Option<Context>,
    /// Serializes project actions of this service.
    pub mu: Mutex<()>,
    /// Only touched while `mu` is held (or by `close`).
    pub client: Mutex<Option<Client>>,
    pub state: RwLock<Selection>,
    pub jobs: Mutex<Jobs>,
    pub job_wait: (Mutex<usize>, Condvar),
    pub progress: Mutex<progress::Progress>,
    /// The remote session of this service's Player, while one is served.
    pub remote: Mutex<Option<crate::remote::Session>>,
}

/// Cheap to clone; clones share one service.
#[derive(Clone)]
pub struct Service(pub(crate) Arc<Inner>);

impl Service {
    pub fn new(options: Options) -> Self {
        Service(Arc::new(Inner {
            project: options.project,
            editor: options.editor,
            host: options.host,
            engine_profile_path: options.engine_profile_path,
            no_editor: options.no_editor,
            asynchronous: options.asynchronous,
            controller: options.controller,
            lifetime: options.lifetime,
            mu: Mutex::new(()),
            client: Mutex::new(None),
            state: RwLock::new(Selection {
                scene: options.scene,
                baseline: options.baseline,
            }),
            jobs: Mutex::default(),
            job_wait: (Mutex::new(0), Condvar::new()),
            progress: Mutex::default(),
            remote: Mutex::default(),
        }))
    }
    pub fn project(&self) -> &str {
        &self.0.project
    }
    pub fn editor(&self) -> &str {
        &self.0.editor
    }
    /// The activated baseline, empty before the first activation.
    pub fn baseline(&self) -> String {
        self.0.state.read().unwrap().baseline.clone()
    }
    pub fn scene(&self) -> String {
        self.0.state.read().unwrap().scene.clone()
    }
    pub(crate) fn select(&self, baseline: &str, scene: &str) {
        let mut state = self.0.state.write().unwrap();
        state.baseline = baseline.into();
        state.scene = scene.into();
    }
    pub(crate) fn lifetime(&self) -> Context {
        self.0.lifetime.clone().unwrap_or_else(Context::background)
    }
    pub(crate) fn client(&self) -> Option<Client> {
        self.0.client.lock().unwrap().clone()
    }
    pub(crate) fn set_client(&self, client: Option<Client>) {
        *self.0.client.lock().unwrap() = client;
    }
}

/// An untyped error carrying a failure code and message.
pub(crate) fn fail(code: &str, message: impl AsRef<str>) -> anyhow::Error {
    util::agent_failure(code, message.as_ref()).into()
}
/// A typed `AuthoringError` with details.
pub(crate) fn fail_with(code: &str, message: &str, details: AnyMap) -> anyhow::Error {
    AuthoringError {
        code: code.into(),
        message: message.into(),
        details: Some(to_serde(&Any::Object(details))),
    }
    .into()
}
/// Untyped errors travel through the authoring layer with the code
/// `internal_error`; every other code is a typed `AuthoringError`.
pub(crate) fn store_error(error: AuthoringError) -> anyhow::Error {
    if error.code != "internal_error" {
        return error.into();
    }
    for kind in [Canceled::Canceled, Canceled::DeadlineExceeded] {
        if kind.to_string() == error.message {
            return kind.into();
        }
    }
    anyhow!(error.message)
}
pub(crate) fn error_text(error: &anyhow::Error) -> String {
    format!("{error:#}")
}
/// Whether the error is a missing file or directory.
pub(crate) fn not_exist(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<sysutil::Error>()
        .is_some_and(sysutil::Error::is_not_exist)
        || error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}
/// Whether the error is a cancellation or an exceeded deadline.
pub(crate) fn context_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Canceled>().is_some()
        || error
            .downcast_ref::<sysutil::Error>()
            .is_some_and(|error| error.is_canceled() || error.is_deadline_exceeded())
}
pub(crate) fn typed(error: &anyhow::Error) -> Option<&AuthoringError> {
    error.downcast_ref::<AuthoringError>()
}
/// Joins two error texts with a newline; `second` may be absent.
pub(crate) fn join_errors(first: &str, second: Option<&anyhow::Error>) -> anyhow::Error {
    match second {
        Some(second) => anyhow!("{first}\n{}", error_text(second)),
        None => anyhow!("{first}"),
    }
}
/// The error an MCP handler reports for `error`.
pub fn handler_error(error: anyhow::Error) -> HandlerError {
    if let Some(detail) = typed(&error) {
        return HandlerError::Typed(detail.clone());
    }
    if context_error(&error) {
        return HandlerError::Cancelled(error_text(&error));
    }
    HandlerError::Failed(error_text(&error))
}

/// An operation id: `^[A-Za-z0-9-]{1,64}$`.
pub(crate) fn operation_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
/// A string member, empty for anything else.
pub(crate) fn text(map: &AnyMap, key: &str) -> String {
    match map.get(key) {
        Some(Any::String(text)) => text.clone(),
        _ => String::new(),
    }
}
/// As `text`, on a value that may not be an object.
pub(crate) fn text_of(value: &Any, key: &str) -> String {
    value
        .as_object()
        .map(|map| text(map, key))
        .unwrap_or_default()
}
pub(crate) use value::map as obj;

pub(crate) fn join(parts: &[&str]) -> String {
    sysutil::join(parts)
}
pub(crate) fn marshal<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    util::marshal(value)
}
/// A reply that is a JSON object with sorted keys.
pub(crate) fn map_reply(map: AnyMap) -> Result<HandlerValue> {
    Ok(HandlerValue::Map(marshal(&Any::Object(map))?))
}
/// A reply that is the JSON of a typed (non-map) result.
pub(crate) fn json_reply<T: Serialize + ?Sized>(value: &T) -> Result<HandlerValue> {
    Ok(HandlerValue::Json(marshal(value)?))
}
pub(crate) fn reply_bytes(value: &HandlerValue) -> &[u8] {
    match value {
        HandlerValue::Nil => b"null",
        HandlerValue::Json(bytes) | HandlerValue::Map(bytes) => bytes,
    }
}
pub(crate) fn to_serde(value: &Any) -> serde_json::Value {
    value
        .to_json()
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Decodes exactly one JSON value; numbers become f64.
pub(crate) fn unmarshal_any(data: &[u8]) -> Result<Any> {
    let (decoded, used) = value::parse_json(data, false)?;
    if !jsondoc::rest_is_space(data, used) {
        // `jsondoc::parse` reports the offending byte; the callers only need the failure.
        jsondoc::parse(data).map_err(|error| anyhow!(error))?;
    }
    Ok(decoded)
}
/// Decodes a JSON object: `None` is the absent map of `null`.
pub(crate) fn unmarshal_map(data: &[u8]) -> Result<Option<AnyMap>> {
    match unmarshal_any(data)? {
        Any::Null => Ok(None),
        Any::Object(map) => Ok(Some(map)),
        other => Err(anyhow!(
            jsondoc::Target::VALUE.mismatch(json_kind(&other), "object")
        )),
    }
}
pub(crate) fn json_kind(value: &Any) -> &'static str {
    match value {
        Any::Null => "null",
        Any::Bool(_) => "bool",
        Any::String(_) => "string",
        Any::Array(_) => "array",
        Any::Object(_) => "object",
        _ => "number",
    }
}
/// Reads a JSON file into a struct; a JSON `null` leaves the default value.
pub(crate) fn read_json<T: DeserializeOwned + Default>(path: &str, limit: u64) -> Result<T> {
    Ok(util::agent_read_json::<Option<T>>(path, limit)?.unwrap_or_default())
}
/// Reads a JSON file into a map (f64 numbers).
pub(crate) fn read_map(path: &str, limit: u64) -> Result<AnyMap> {
    let data = util::read_agent_file(path, limit)?;
    let (decoded, used) = value::parse_json(&data, false)?;
    if !jsondoc::rest_is_space(&data, used) {
        return Err(anyhow!("unexpected JSON content"));
    }
    match decoded {
        Any::Null => Ok(AnyMap::new()),
        Any::Object(map) => Ok(map),
        other => Err(anyhow!(
            jsondoc::Target::VALUE.mismatch(json_kind(&other), "object")
        )),
    }
}
/// True when the path itself (not a link target) is a regular file.
pub(crate) fn regular(path: &str) -> bool {
    sysutil::lstat(path).is_ok_and(|info| info.is_regular())
}
/// Stats a path without following links, with a `lstat <path>: <reason>` error.
pub(crate) fn lstat(path: &str) -> Result<sysutil::Stat> {
    sysutil::lstat(path).map_err(|error| sysutil::Error::os("lstat", path, &error).into())
}
/// Stats a path, following links, with a `stat <path>: <reason>` error.
pub(crate) fn stat(path: &str) -> Result<sysutil::Stat> {
    sysutil::stat(path).map_err(|error| sysutil::Error::os("stat", path, &error).into())
}
/// True for an already clean `Assets/...unity` scene path.
pub(crate) fn authored_scene(scene: &str) -> bool {
    scene.starts_with("Assets/")
        && sysutil::clean(scene) == scene
        && sysutil::ext(scene) == ".unity"
}
