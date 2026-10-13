//! The tool and resource set of the server, independent of the MCP transport.
use super::{
    Options,
    backend::{BackendData, HandlerError, RuntimeHandler, call_backend, object_value, text},
    jsonschema::Resolved,
    reply::{Data, ResourceResult, ToolResult, reply, resource_reply},
    schema,
};
use crate::{
    authoring::{
        AssetCreateRequest, AuthoringError, EditRequest, HierarchyRequest, InventoryRequest,
        PublicationUndoRequest, ScriptRequest, Source, Store, UndoRequest, native_component_types,
    },
    cancel::{self, Context, Flag},
    jsondoc::{self, Json, Member, Target},
    sha256,
};
use regex::Regex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub const SERVER_NAME: &str = "unity-micro-editor";
pub const SERVER_VERSION: &str = "0.2.0";
pub const INSTRUCTIONS: &str = "This service edits authored Unity project files. Read sourceRevision or asset_read revision before each write and send a unique operationId. Resolved hierarchy revision binds the entire prefab dependency graph; composite IDs are read-only and provenance identifies editable local/source IDs. Repeating identical arguments with the same operationId safely replays the original result. Source writes preserve .meta GUIDs. Source write success does not imply compilation, import or Player success. Use explicit source prefab override operations or prefab_edit for source prefab authoring. Read project_capabilities to inspect supported behavior.";
/// Added to [`INSTRUCTIONS`] by a server that controls a Player session.
const SESSION_INSTRUCTIONS: &str = " With a running Player session a saved C# edit is compiled and applied to the running Player when possible; the Player keeps its process and its game state, and session_control(wait_update) reports the outcome. When a reply has the error code full_recompile_required, or the session state has hotReload.fullRecompileRequired true, the edit cannot be applied live and the Player still runs the previous code: call hot_reload with action recompile. A recompile restarts the Player and resets game state.";
// What the same server adds to the two tools that write C# source. A server
// without a session has neither tool these sentences name.
const SCRIPT_CREATE_SESSION: &str = " A running Player session compiles the file. A type it adds cannot be applied live: unless the session restarts by itself (onUnsupportedEdit restart or hotReload false), session_control(wait_update) fails with full_recompile_required and hot_reload(recompile) restarts the Player on the new code.";
const SCRIPT_EDIT_SESSION: &str = " A running Player session compiles the saved change and applies it to the running Player when possible, keeping game state; session_control(wait_update) reports the outcome, and its error code full_recompile_required means hot_reload(recompile).";
pub const PROJECT_URI: &str = "unitymicroeditor://project";
pub const CAPABILITIES_URI: &str = "unitymicroeditor://capabilities";

const PROJECT_TOOL_SUFFIX: &str = " Invoke a publicly attributed or registered project method. parameters follows its DTO schema; target is required for component methods. This wrapper is pinned to the catalog Player PID/revision when caller guards are omitted.";
const RUNTIME_OPERATIONS: &str = "status,hierarchy,inspect,logs,raycast,tools,screenshot,pause,resume,wait_frames,custom,invoke,simulation_pause,simulation_resume,simulation_step,input";

/// A tool as `tools/list` reports it. Annotations are
/// `readOnlyHint`, `idempotentHint` and `openWorldHint: false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON text in the member order `tools/list` reports.
    pub input_schema: String,
    /// The `Reply` schema; absent for `project.<name>` tools.
    pub output_schema: Option<String>,
    pub read_only: bool,
    pub idempotent: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDefinition {
    pub uri: &'static str,
    pub name: &'static str,
    pub mime_type: &'static str,
}

/// A request answered with a JSON-RPC error instead of a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// `unknown tool "<name>"`, code -32602.
    UnknownTool(String),
    /// `Resource not found`, code -32002, data `{"uri": ...}`.
    ResourceNotFound(String),
    /// The error text of a failed resource handler or reply encoding.
    Failed(String),
}
impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::UnknownTool(name) => {
                write!(f, "unknown tool {}", jsondoc::quoted_text(name))
            }
            CallError::ResourceNotFound(_) => f.write_str("Resource not found"),
            CallError::Failed(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for CallError {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Inspect,
    Capabilities,
    Inventory,
    AssetRead,
    Hierarchy,
    SceneEdit,
    AssetCreate,
    ScriptRead,
    ScriptCreate,
    ScriptEdit,
    SourceUndo,
    ProjectTools,
    SessionControl,
    RuntimeQuery,
    RuntimeCommand,
    RemoteSession,
    HotReload,
    ProjectPrepare,
    ProjectBuild,
    PrefabEdit,
    ComponentSchema,
    ComponentAdd,
}
struct StaticTool {
    definition: ToolDefinition,
    schema: Value,
    resolved: Resolved,
    /// The input type name shown in decoding type errors.
    target_type: &'static str,
    /// Fields that keep their raw JSON text.
    raw_fields: &'static [&'static str],
    kind: Kind,
}
struct DynamicTool {
    definition: ToolDefinition,
    name: String,
    resolved: Resolved,
    pid: i64,
    revision: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ReadInput {
    pub path: String,
    pub offset: i64,
    pub limit: i64,
    pub metadata_offset: i64,
    pub metadata_limit: i64,
    pub expected_revision: String,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePage {
    #[serde(flatten)]
    pub source: Source,
    pub offset: i64,
    pub full_length: usize,
    pub next_offset: Option<usize>,
    pub truncated: bool,
    pub metadata_offset: i64,
    pub metadata_full_length: usize,
    pub metadata_next_offset: Option<usize>,
    pub metadata_truncated: bool,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
struct UndoInput {
    path: String,
    expected_revision: String,
    operation_id: String,
    original_operation_id: String,
    expected_revisions: Option<BTreeMap<String, String>>,
}
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct SessionInput {
    pub action: String,
    pub after_sequence: i64,
    pub request_id: String,
    pub expected_pid: i64,
    pub expected_revision: String,
    pub arguments: Option<Map<String, Value>>,
}
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct RuntimeInput {
    pub operation: String,
    pub arguments: Option<Map<String, Value>>,
    pub request_id: String,
    pub expected_pid: i64,
    pub expected_revision: String,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
struct RemoteInput {
    action: String,
    listen: String,
    transport: String,
    webrtc_address: String,
    webrtc_port: i64,
    fps: i64,
    max_width: i64,
    bitrate_kbps: i64,
    publish: String,
    publish_pattern: String,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
struct HotReloadInput {
    action: String,
    request_id: String,
}
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProjectToolsInput {
    pub request_id: String,
    pub expected_pid: i64,
    pub expected_revision: String,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
struct ProjectPrepareInput {
    scene: String,
    request_id: String,
    host_player: String,
    native_catalog: String,
    engine_profile: String,
    activate: Option<bool>,
    rebuild_host: bool,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
struct ProjectBuildInput {
    scenes: Option<Vec<String>>,
    development: bool,
    request_id: String,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
struct PrefabEditInput {
    action: String,
    path: String,
    expected_revision: String,
    operation_id: String,
    request_id: String,
    file_id: String,
    target_path: String,
    prefab_path: String,
    output_path: String,
    parent_file_id: String,
    parent_path: String,
    name: String,
    completely: bool,
    component_type: String,
    property: String,
    #[serde(deserialize_with = "raw_message")]
    value: Option<Box<serde_json::value::RawValue>>,
    fields: Option<Map<String, Value>>,
}
// A present raw JSON field keeps its bytes, `null` included.
fn raw_message<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Box<serde_json::value::RawValue>>, D::Error> {
    Box::<serde_json::value::RawValue>::deserialize(deserializer).map(Some)
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
struct ComponentSchemaInput {
    component_type: String,
    request_id: String,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
struct ComponentAddInput {
    path: String,
    file_id: String,
    component_type: String,
    expected_revision: String,
    operation_id: String,
    request_id: String,
    fields: Option<Map<String, Value>>,
}

/// Compact JSON of a struct: members in declaration order.
struct Encoder(String);
impl Encoder {
    fn new() -> Self {
        Encoder(String::from("{"))
    }
    fn key(&mut self, key: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        self.0.push_str(&jsondoc::quote(key));
        self.0.push(':');
    }
    fn raw(&mut self, key: &str, value: &str) {
        self.key(key);
        self.0.push_str(value);
    }
    fn text(&mut self, key: &str, value: &str) {
        self.raw(key, &jsondoc::quote(value));
    }
    fn text_omitempty(&mut self, key: &str, value: &str) {
        if !value.is_empty() {
            self.text(key, value);
        }
    }
    fn int_omitempty(&mut self, key: &str, value: i64) {
        if value != 0 {
            self.raw(key, &value.to_string());
        }
    }
    fn bool_omitempty(&mut self, key: &str, value: bool) {
        if value {
            self.raw(key, "true");
        }
    }
    // An object member that is omitted when absent or empty.
    fn map_omitempty(&mut self, key: &str, value: &Option<Map<String, Value>>) {
        if let Some(map) = value
            && !map.is_empty()
        {
            self.raw(
                key,
                &String::from_utf8(jsondoc::marshal(map).expect("JSON values always serialize"))
                    .expect("JSON is UTF-8"),
            );
        }
    }
    fn finish(mut self) -> Vec<u8> {
        self.0.push('}');
        self.0.into_bytes()
    }
}
impl SessionInput {
    fn marshal(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.text("action", &self.action);
        out.int_omitempty("afterSequence", self.after_sequence);
        out.text_omitempty("requestId", &self.request_id);
        out.int_omitempty("expectedPid", self.expected_pid);
        out.text_omitempty("expectedRevision", &self.expected_revision);
        out.map_omitempty("arguments", &self.arguments);
        out.finish()
    }
}
// `RuntimeInput` with its arguments already encoded (`None` omits them).
fn runtime_input(
    operation: &str,
    arguments: Option<&str>,
    request_id: &str,
    expected_pid: i64,
    expected_revision: &str,
) -> Vec<u8> {
    let mut out = Encoder::new();
    out.text("operation", operation);
    if let Some(arguments) = arguments {
        out.raw("arguments", arguments);
    }
    out.text_omitempty("requestId", request_id);
    out.int_omitempty("expectedPid", expected_pid);
    out.text_omitempty("expectedRevision", expected_revision);
    out.finish()
}
impl RuntimeInput {
    fn marshal(&self) -> Vec<u8> {
        let mut arguments = Encoder::new();
        arguments.map_omitempty("arguments", &self.arguments);
        let arguments = String::from_utf8(arguments.finish()).expect("JSON is UTF-8");
        // `{"arguments":<map>}` or `{}` when the map is absent or empty.
        let arguments = arguments
            .strip_prefix("{\"arguments\":")
            .and_then(|rest| rest.strip_suffix('}'));
        runtime_input(
            &self.operation,
            arguments,
            &self.request_id,
            self.expected_pid,
            &self.expected_revision,
        )
    }
}
impl RemoteInput {
    fn marshal(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.text("action", &self.action);
        out.text_omitempty("listen", &self.listen);
        out.text_omitempty("transport", &self.transport);
        out.text_omitempty("webrtcAddress", &self.webrtc_address);
        out.int_omitempty("webrtcPort", self.webrtc_port);
        out.int_omitempty("fps", self.fps);
        out.int_omitempty("maxWidth", self.max_width);
        out.int_omitempty("bitrateKbps", self.bitrate_kbps);
        out.text_omitempty("publish", &self.publish);
        out.text_omitempty("publishPattern", &self.publish_pattern);
        out.finish()
    }
}
impl HotReloadInput {
    fn marshal(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.text("action", &self.action);
        out.text_omitempty("requestId", &self.request_id);
        out.finish()
    }
}
impl ProjectPrepareInput {
    fn marshal(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.text("scene", &self.scene);
        out.text("requestId", &self.request_id);
        out.text_omitempty("hostPlayer", &self.host_player);
        out.text_omitempty("nativeCatalog", &self.native_catalog);
        out.text_omitempty("engineProfile", &self.engine_profile);
        if let Some(activate) = self.activate {
            out.raw("activate", if activate { "true" } else { "false" });
        }
        out.bool_omitempty("rebuildHost", self.rebuild_host);
        out.finish()
    }
}
impl ProjectBuildInput {
    fn marshal(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        match &self.scenes {
            None => out.raw("scenes", "null"),
            Some(scenes) => out.raw(
                "scenes",
                &String::from_utf8(jsondoc::marshal(scenes).expect("strings serialize"))
                    .expect("JSON is UTF-8"),
            ),
        }
        // Optional input with an explicit false on the backend wire.
        out.raw(
            "development",
            if self.development { "true" } else { "false" },
        );
        out.text("requestId", &self.request_id);
        out.finish()
    }
}
impl PrefabEditInput {
    fn marshal(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.text("action", &self.action);
        out.text("path", &self.path);
        out.text("expectedRevision", &self.expected_revision);
        out.text("operationId", &self.operation_id);
        out.text_omitempty("requestId", &self.request_id);
        out.text_omitempty("fileId", &self.file_id);
        out.text_omitempty("targetPath", &self.target_path);
        out.text_omitempty("prefabPath", &self.prefab_path);
        out.text_omitempty("outputPath", &self.output_path);
        out.text_omitempty("parentFileId", &self.parent_file_id);
        out.text_omitempty("parentPath", &self.parent_path);
        out.text_omitempty("name", &self.name);
        out.bool_omitempty("completely", self.completely);
        out.text_omitempty("componentType", &self.component_type);
        out.text_omitempty("property", &self.property);
        if let Some(value) = &self.value {
            out.raw("value", value.get());
        }
        out.map_omitempty("fields", &self.fields);
        out.finish()
    }
}
impl ComponentSchemaInput {
    fn marshal(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.text("componentType", &self.component_type);
        out.text("requestId", &self.request_id);
        out.finish()
    }
}
impl ComponentAddInput {
    fn marshal(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.text("path", &self.path);
        out.text("fileId", &self.file_id);
        out.text("componentType", &self.component_type);
        out.text("expectedRevision", &self.expected_revision);
        out.text("operationId", &self.operation_id);
        out.text_omitempty("requestId", &self.request_id);
        out.map_omitempty("fields", &self.fields);
        out.finish()
    }
}

fn invalid_request(message: impl Into<String>) -> HandlerError {
    HandlerError::Typed(AuthoringError {
        code: "invalid_request".into(),
        message: message.into(),
        details: None,
    })
}
fn typed(code: &str, message: &str) -> HandlerError {
    HandlerError::Typed(AuthoringError {
        code: code.into(),
        message: message.into(),
        details: None,
    })
}

fn slice_text(
    text: &str,
    offset: i64,
    limit: i64,
    default_limit: i64,
) -> Result<(String, Option<usize>, bool), AuthoringError> {
    let limit = if limit == 0 { default_limit } else { limit };
    let invalid = |message: &str| AuthoringError {
        code: "invalid_request".into(),
        message: message.into(),
        details: None,
    };
    if offset < 0 || offset as u64 > text.len() as u64 || !(4..=1 << 20).contains(&limit) {
        return Err(invalid(
            "read offset must be within the text and page limit from4 to1048576 bytes",
        ));
    }
    let offset = offset as usize;
    if !text.is_char_boundary(offset) {
        return Err(invalid("read offset must be a UTF-8 character boundary"));
    }
    let mut end = (offset + limit as usize).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok((
        text[offset..end].into(),
        (end < text.len()).then_some(end),
        offset != 0 || end < text.len(),
    ))
}
pub fn read_page(mut source: Source, input: &ReadInput) -> Result<SourcePage, AuthoringError> {
    if !input.expected_revision.is_empty() && source.revision != input.expected_revision {
        return Err(AuthoringError {
            code: "revision_conflict".into(),
            message: "authored source or metadata changed between pages".into(),
            details: Some(
                json!({"actualRevision": source.revision, "expectedRevision": input.expected_revision}),
            ),
        });
    }
    let full_length = source.content.len();
    let metadata_full_length = source.metadata.len();
    let (content, next_offset, truncated) =
        slice_text(&source.content, input.offset, input.limit, 262144)?;
    let (metadata, metadata_next_offset, metadata_truncated) = slice_text(
        &source.metadata,
        input.metadata_offset,
        input.metadata_limit,
        65536,
    )?;
    source.content = content;
    source.metadata = metadata;
    Ok(SourcePage {
        source,
        offset: input.offset,
        full_length,
        next_offset,
        truncated,
        metadata_offset: input.metadata_offset,
        metadata_full_length,
        metadata_next_offset,
        metadata_truncated,
    })
}

// The last member with this key: the last of duplicate keys wins.
fn last<'a>(members: &'a [Member], key: &str) -> Option<&'a Member> {
    members.iter().rev().find(|member| member.key == key)
}
pub(crate) fn encode_tree(json: &Json, out: &mut Vec<u8>) {
    match json {
        Json::Null => out.extend_from_slice(b"null"),
        Json::Bool(flag) => out.extend_from_slice(if *flag { b"true" } else { b"false" }),
        Json::Number(text) => out.extend_from_slice(text.as_bytes()),
        Json::String(text) => {
            out.extend_from_slice(serde_json::to_string(text).expect("string").as_bytes())
        }
        Json::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                encode_tree(item, out);
            }
            out.push(b']');
        }
        Json::Object(members) => {
            out.push(b'{');
            let mut first = true;
            for (index, member) in members.iter().enumerate() {
                if members[index + 1..]
                    .iter()
                    .any(|later| later.key == member.key)
                {
                    continue;
                }
                if !std::mem::take(&mut first) {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(&member.key).expect("key").as_bytes());
                out.push(b':');
                encode_tree(&member.value, out);
            }
            out.push(b'}');
        }
    }
}
/// The arguments as they are applied to the input struct: the last of
/// duplicate keys wins, a top-level `null` leaves its field at the zero value,
/// and raw JSON fields keep their compacted bytes.
fn normalize(json: &Json, raw: &[u8], raw_fields: &[&str]) -> Vec<u8> {
    let Json::Object(members) = json else {
        let mut out = Vec::new();
        encode_tree(json, &mut out);
        return out;
    };
    let mut out = vec![b'{'];
    for (index, member) in members.iter().enumerate() {
        if members[index + 1..]
            .iter()
            .any(|later| later.key == member.key)
        {
            continue;
        }
        let verbatim = raw_fields.contains(&member.key.as_str());
        if !verbatim && member.value == Json::Null {
            continue;
        }
        if out.len() > 1 {
            out.push(b',');
        }
        out.extend_from_slice(serde_json::to_string(&member.key).expect("key").as_bytes());
        out.push(b':');
        if verbatim {
            out.extend(jsondoc::compact(member.raw(raw)));
        } else {
            encode_tree(&member.value, &mut out);
        }
    }
    out.push(b'}');
    out
}
/// The type error for a number that validates as a JSON Schema integer
/// (`1.0`, `1e2`, out of range) yet is not a 64-bit integer.
fn check_integers(
    schema: &Value,
    json: &Json,
    target_type: &str,
    path: &str,
) -> Result<(), String> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };
    let Json::Object(members) = json else {
        return Ok(());
    };
    for member in members {
        let Some(property) = properties.get(&member.key) else {
            continue;
        };
        let field = if path.is_empty() {
            member.key.clone()
        } else {
            format!("{path}.{}", member.key)
        };
        match &member.value {
            Json::Number(literal)
                if property.get("type") == Some(&Value::String("integer".into()))
                    && literal.parse::<i64>().is_err() =>
            {
                let kind = if member.key == "afterSequence" {
                    "int64"
                } else {
                    "int"
                };
                return Err(Target {
                    strukt: target_type,
                    field: &field,
                }
                .mismatch(&format!("number {literal}"), kind));
            }
            Json::Array(items) => {
                if let Some(element) = property.get("items") {
                    for item in items {
                        // The error names the struct that declares the field.
                        let element_type = if member.key == "operations" {
                            "Operation"
                        } else {
                            target_type
                        };
                        check_integers(element, item, element_type, &field)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn tool_name() -> &'static Regex {
    static NAME: OnceLock<Regex> = OnceLock::new();
    NAME.get_or_init(|| Regex::new(r"^[A-Za-z0-9_.-]{1,120}$").unwrap())
}
// The description cut to 65536 bytes and then encoded as JSON: every byte of a
// cut character becomes U+FFFD.
fn truncate_description(description: &str) -> String {
    if description.len() <= 65536 {
        return description.to_string();
    }
    let bytes = &description.as_bytes()[..65536];
    let valid = match std::str::from_utf8(bytes) {
        Ok(text) => return text.to_string(),
        Err(error) => error.valid_up_to(),
    };
    let mut text = String::from_utf8_lossy(&bytes[..valid]).into_owned();
    text.extend(std::iter::repeat_n('\u{fffd}', bytes.len() - valid));
    text
}

type Listener = Arc<dyn Fn() + Send + Sync>;
struct Inner {
    store: Store,
    runtime: Option<Arc<dyn RuntimeHandler>>,
    project_actions: Option<Arc<dyn RuntimeHandler>>,
    backends: Value,
    tools: Vec<StaticTool>,
    /// `project.<name>` tools currently registered on the server.
    dynamic: Mutex<BTreeMap<String, Arc<DynamicTool>>>,
    /// Fingerprints of the registered `project.<name>` tools.
    catalog: Mutex<BTreeMap<String, String>>,
    listeners: Mutex<Vec<Listener>>,
}
/// The MCP server, without its transport.
#[derive(Clone)]
pub struct Server(Arc<Inner>);

fn read_tool(name: &str, description: &str, input_schema: String) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema,
        output_schema: Some(schema::reply()),
        read_only: true,
        idempotent: false,
    }
}
fn write_tool(
    name: &str,
    description: &str,
    idempotent: bool,
    input_schema: String,
) -> ToolDefinition {
    ToolDefinition {
        read_only: false,
        idempotent,
        ..read_tool(name, description, input_schema)
    }
}
fn static_tool(
    definition: ToolDefinition,
    target_type: &'static str,
    raw_fields: &'static [&'static str],
    kind: Kind,
) -> StaticTool {
    let schema: Value = serde_json::from_str(&definition.input_schema)
        .unwrap_or_else(|error| panic!("{} schema: {error}", definition.name));
    // Registration uses fixed schemas, so a bad one panics.
    let resolved = Resolved::parse(&schema)
        .unwrap_or_else(|error| panic!("{} schema: {error}", definition.name));
    StaticTool {
        definition,
        schema,
        resolved,
        target_type,
        raw_fields,
        kind,
    }
}

fn source_tools(session: bool) -> Vec<StaticTool> {
    use schema::{ANY, array, boolean, integer, map, object, optional_boolean, string};
    let with_session = |description: &str, addition: &str| match session {
        true => format!("{description}{addition}"),
        false => description.to_string(),
    };
    let read_input = || {
        object(&[
            (
                "path",
                string("normalized authored path beneath Assets"),
                true,
            ),
            ("offset", integer("UTF-8 byte offset, default zero"), false),
            (
                "limit",
                integer("source bytes per page, default262144, minimum4 maximum1048576"),
                false,
            ),
            ("metadataOffset", integer(""), false),
            (
                "metadataLimit",
                integer("metadata bytes per page, default65536, minimum4 maximum1048576"),
                false,
            ),
            ("expectedRevision", string(""), false),
        ])
    };
    let script_request = || {
        object(&[
            ("path", string(""), true),
            (
                "expectedRevision",
                string("use absent only when creating a new script"),
                true,
            ),
            ("operationId", string(""), true),
            ("content", string(""), true),
            (
                "guid",
                string("optional lowercase 32-hex GUID for a newly created script"),
                false,
            ),
        ])
    };
    let operation = object(&[
        (
            "action",
            string(
                "create_gameobject,delete_gameobject,reparent,set_property,add_component,remove_component,instantiate_prefab,set_prefab_override,revert_prefab_override",
            ),
            true,
        ),
        ("fileId", string(""), false),
        ("key", string(""), false),
        ("name", string(""), false),
        (
            "parentId",
            string("parent GameObject fileID or 0 for root; local transform is preserved"),
            false,
        ),
        ("property", string(""), false),
        ("value", ANY.into(), false),
        ("scriptPath", string(""), false),
        ("componentType", string(""), false),
        ("classId", integer(""), false),
        ("templatePath", string(""), false),
        ("templateFileId", string(""), false),
        ("templateRevision", string(""), false),
        ("prefabPath", string(""), false),
        ("prefabRevision", string(""), false),
        ("instanceId", string(""), false),
        ("targetGuid", string(""), false),
        ("targetFileId", string(""), false),
        ("objectReference", map(ANY, ""), false),
        ("dependencyRevisions", map(&string(""), ""), false),
        ("rectTransform", boolean(), false),
        ("fields", map(ANY, ""), false),
        ("recursive", boolean(), false),
    ]);
    vec![
        static_tool(
            read_tool(
                "project_inspect",
                "Inspect the explicit target project, Unity profile, scenes, prefabs, scripts, assembly definitions and identity issues.",
                object(&[]),
            ),
            "",
            &[],
            Kind::Inspect,
        ),
        static_tool(
            read_tool(
                "project_capabilities",
                "Read supported authoring operations, guarantees, limitations, and whether runtime control is configured.",
                object(&[]),
            ),
            "",
            &[],
            Kind::Capabilities,
        ),
        static_tool(
            read_tool(
                "project_inventory",
                "Read a filtered page of ordinary Assets files, including native inputs, GUIDs, metadata hashes and identity issues. Default100, maximum500. Inventory revision binds paths/stat/metadata; includeSourceRevision provides exact editable digests for supported text assets.",
                object(&[
                    ("query", string(""), false),
                    ("pathPrefix", string(""), false),
                    ("extensions", array(&string(""), ""), false),
                    ("offset", integer(""), false),
                    ("limit", integer(""), false),
                    ("expectedRevision", string(""), false),
                    ("includeSourceRevision", boolean(), false),
                ]),
            ),
            "InventoryRequest",
            &[],
            Kind::Inventory,
        ),
        static_tool(
            read_tool(
                "asset_read",
                "Read exact pages of authored source and metadata with their full revision. Byte offset/limit default 0/262144; metadataOffset/metadataLimit default 0/65536. Follow nextOffset/metadataNextOffset; expectedRevision can bind later pages to the first.",
                read_input(),
            ),
            "ReadInput",
            &[],
            Kind::AssetRead,
        ),
        static_tool(
            read_tool(
                "scene_hierarchy",
                "Read a page of GameObjects/components, resolving nested authored prefabs and variants by default. Default100, maximum500. Resolved composite IDs are read-only; provenance reports original GUID/fileIDs and editable local IDs. Use sourceRevision for writes, dependencyRevisions for prefab operations and graph revision for subsequent pages. resolvePrefabs:false returns raw authored records.",
                object(&[
                    ("path", string(""), true),
                    (
                        "query",
                        string(
                            "optional case-insensitive name substring or exact GameObject fileID",
                        ),
                        false,
                    ),
                    ("offset", integer(""), false),
                    (
                        "limit",
                        integer("object page length, default 100, maximum 500"),
                        false,
                    ),
                    (
                        "includeFields",
                        optional_boolean(
                            "default true; false omits serialized component field payloads",
                        ),
                        false,
                    ),
                    ("expectedRevision", string(""), false),
                    (
                        "resolvePrefabs",
                        optional_boolean(
                            "default true; false returns only authored records and unresolved prefab links",
                        ),
                        false,
                    ),
                ]),
            ),
            "hierarchyInput",
            &[],
            Kind::Hierarchy,
        ),
        static_tool(
            write_tool(
                "scene_edit",
                "Apply a revision-checked source transaction or preview. Actions: create_gameobject (optional explicit rectTransform fields), delete_gameobject, reparent, set_property (scalar/reference/collection), add_component (scriptPath or supported componentType/classId with explicit fields/template), remove_component, instantiate_prefab, set_prefab_override, revert_prefab_override. Creation keys use $key/$key.transform/$key.instance. Reparent keeps local transform; prefab operations bind dependencyRevisions. Complex prefab operations use prefab_edit. Unrelated YAML spans and metadata remain intact.",
                true,
                object(&[
                    ("path", string(""), true),
                    ("expectedRevision", string(""), true),
                    ("operationId", string(""), true),
                    ("operations", array(&operation, ""), true),
                    ("preview", boolean(), false),
                ]),
            ),
            "EditRequest",
            &[],
            Kind::SceneEdit,
        ),
        static_tool(
            write_tool(
                "asset_create",
                "Create an absent .mat, ScriptableObject .asset or authored .prefab from explicit Unity YAML plus a new GUID-preserving metadata identity. expectedRevision must be absent; parent directory exists. Does not invent shader/script defaults or confirm runtime loading.",
                true,
                object(&[
                    ("path", string(""), true),
                    ("expectedRevision", string(""), true),
                    ("operationId", string(""), true),
                    ("content", string(""), true),
                    ("guid", string(""), false),
                ]),
            ),
            "AssetCreateRequest",
            &[],
            Kind::AssetCreate,
        ),
        static_tool(
            read_tool(
                "script_read",
                "Read a C# source page, existing metadata GUID and full revision. Paging is identical to asset_read.",
                read_input(),
            ),
            "ReadInput",
            &[],
            Kind::ScriptRead,
        ),
        static_tool(
            write_tool(
                "script_create",
                &with_session(
                    "Create a new C# file and MonoImporter .meta transactionally. expectedRevision must be absent; parent directory must exist. Does not run the compiler.",
                    SCRIPT_CREATE_SESSION,
                ),
                true,
                script_request(),
            ),
            "ScriptRequest",
            &[],
            Kind::ScriptCreate,
        ),
        static_tool(
            write_tool(
                "script_edit",
                &with_session(
                    "Replace C# source using expectedRevision and operationId; preserve existing .meta byte-for-byte. Does not run the compiler.",
                    SCRIPT_EDIT_SESSION,
                ),
                true,
                script_request(),
            ),
            "ScriptRequest",
            &[],
            Kind::ScriptEdit,
        ),
        static_tool(
            write_tool(
                "source_undo",
                "Undo an existing-file edit with path/expectedRevision, or a staged multi-file publication with expectedRevisions. Supply originalOperationId and new operationId. Refuses intervening changes, referenced-object deletion and erasing newly created files.",
                true,
                object(&[
                    ("path", string(""), false),
                    ("expectedRevision", string(""), false),
                    ("operationId", string(""), true),
                    ("originalOperationId", string(""), true),
                    (
                        "expectedRevisions",
                        map(
                            &string(""),
                            "for a staged multi-file publication; one exact resulting revision per changed asset",
                        ),
                        false,
                    ),
                ]),
            ),
            "undoInput",
            &[],
            Kind::SourceUndo,
        ),
    ]
}
fn runtime_tools() -> Vec<StaticTool> {
    use schema::{ANY, integer, map, object, string};
    let runtime_input = || {
        object(&[
            ("operation", string(RUNTIME_OPERATIONS), true),
            ("arguments", map(ANY, ""), false),
            ("requestId", string(""), false),
            ("expectedPid", integer(""), false),
            ("expectedRevision", string(""), false),
        ])
    };
    vec![
        static_tool(
            read_tool(
                "project_tools",
                "Discover public attributed/registered project tools from the current Player, publish project.<name> MCP tools using exact DTO parameter schemas and readonly hints, and remove stale catalog entries. Discovery errors remain visible in the runtime receipt.",
                object(&[
                    ("requestId", string(""), false),
                    ("expectedPid", integer(""), false),
                    ("expectedRevision", string(""), false),
                ]),
            ),
            "ProjectToolsInput",
            &[],
            Kind::ProjectTools,
        ),
        static_tool(
            write_tool(
                "session_control",
                "Control the configured Player session: start, stop, status, reset, wait, wait_update. Start can be asynchronous: while it prepares, start, status and wait report the preparation stage and log path. Wait for confirmed readiness. wait_update returns when the session applied or rejected a source change after afterSequence; read status.lastSequence before the edit and pass it. wait returns when a Player is ready after afterSequence: after a start or a restart, and with the same outcome of an edit as wait_update, so it does not outlast one. A C# edit is applied to the running Player when possible: same pid, game state kept, hotReload.codeGeneration advanced. An edit that cannot be applied live fails with the error code full_recompile_required, whose details list the members and reasons, and the Player keeps the previous code. While hotReload.fullRecompileRequired is true neither wait nor wait_update succeeds, whatever afterSequence is and also for a scene edit that was applied since; call hot_reload with action recompile, which restarts the Player and resets game state. start/reset arguments: scene, gameArguments (the game's own command line, like the CLI's -- args), contentScenes (GUID=Assets/...unity), contentPrefabs (Assets/...prefab), rebuildHost, hotReload (default true; false restarts the Player on every C# edit), onUnsupportedEdit (report, the default, or restart: the Player restarts by itself instead of failing with full_recompile_required). expectedPid/revision guard stale runtime sessions.",
                false,
                object(&[
                    (
                        "action",
                        string("start,stop,status,reset,wait,wait_update"),
                        true,
                    ),
                    ("afterSequence", integer(""), false),
                    ("requestId", string(""), false),
                    ("expectedPid", integer(""), false),
                    ("expectedRevision", string(""), false),
                    (
                        "arguments",
                        map(
                            ANY,
                            "start/reset: scene, gameArguments[], contentScenes[], contentPrefabs[], addressables, rebuildHost, hotReload, onUnsupportedEdit",
                        ),
                        false,
                    ),
                ]),
            ),
            "SessionInput",
            &[],
            Kind::SessionControl,
        ),
        static_tool(
            read_tool(
                "runtime_query",
                "Query configured Player via public Unity APIs. Read-only operations: status, hierarchy, inspect, logs, raycast, tools. tools refreshes the public project.<name> catalog. Object references must be validated against the current session PID/revision.",
                runtime_input(),
            ),
            "RuntimeInput",
            &[],
            Kind::RuntimeQuery,
        ),
        static_tool(
            write_tool(
                "runtime_command",
                "Run configured Player operations: screenshot,pause,resume,wait_frames,custom,invoke,simulation_pause,simulation_resume,simulation_step,input. screenshot defaults to the full viewport with overlay UI; arguments.mode=camera captures a selected camera with optional width/height. pause affects scaled time; simulation_* uses the installed game-loop gate. invoke calls only public attributed/registered tools. input uses the configured synthetic device path. Supply requestId and current PID/revision; pending/unknown requires retry of the SAME requestId.",
                false,
                runtime_input(),
            ),
            "RuntimeInput",
            &[],
            Kind::RuntimeCommand,
        ),
        static_tool(
            write_tool(
                "remote_session",
                "Serve the Player's picture and input to a browser: start, stop, status. start binds an HTTP listener of its own (default 127.0.0.1 and a free port, so every editor gets a distinct address) and returns url, which carries the session's secret; open it in a browser to watch the game as hardware-encoded H.264 and play it with keyboard and mouse. It can be started before the Player: the page waits and follows Player restarts. Encoding needs an NVIDIA GPU. transport websocket (default) uses one HTTP port and needs a WebCodecs browser on localhost or HTTPS; transport webrtc also works on a plain-HTTP page but sends the video over a UDP port. publish runs a command for the session's lifetime (a tunnel client, for example) and the reply's publicUrl is the address it announced.",
                false,
                object(&[
                    ("action", string("start,stop,status"), true),
                    (
                        "listen",
                        string(
                            "start: literal IP and port of the HTTP listener, default 127.0.0.1:0",
                        ),
                        false,
                    ),
                    (
                        "transport",
                        string("start: websocket (default) or webrtc"),
                        false,
                    ),
                    (
                        "webrtcAddress",
                        string(
                            "start, webrtc: local IP of the UDP socket; default the listener's, or the outbound one",
                        ),
                        false,
                    ),
                    (
                        "webrtcPort",
                        integer("start, webrtc: UDP port, default a free one"),
                        false,
                    ),
                    (
                        "publish",
                        string(
                            "start: command with {url}, {address}, {host} or {port} of the listener, e.g. a tunnel client; off disables the server's default",
                        ),
                        false,
                    ),
                    (
                        "publishPattern",
                        string(
                            "start: regular expression whose first group is the public address in the command's output; default the first non-local http(s) URL",
                        ),
                        false,
                    ),
                    (
                        "fps",
                        integer(
                            "start: capture rate, 1-60, default 60; bounded by the Player's own frame rate",
                        ),
                        false,
                    ),
                    (
                        "maxWidth",
                        integer("start: widest encoded picture, 64-4096, default 1920"),
                        false,
                    ),
                    (
                        "bitrateKbps",
                        integer("start: video bitrate, 250-100000, default 12000"),
                        false,
                    ),
                ]),
            ),
            "RemoteInput",
            &[],
            Kind::RemoteSession,
        ),
        static_tool(
            write_tool(
                "hot_reload",
                "C# hot reload of the Player session: status, recompile. A saved C# edit is compiled and applied to the running Player when possible; the Player keeps its process and game state, and session_control(wait_update) reports it. An edit that cannot be applied live is never approximated: the Player keeps the previous code, wait_update fails with full_recompile_required and the session state has hotReload.fullRecompileRequired true with the unsupported members and reasons, until a recompile. recompile restarts the Player on the newest compiled code and resets game state; it returns the new session state like wait_update, or the compile error when the sources do not compile, which keeps the Player. It also works for a session started with hotReload false. One recompile runs at a time. requestId names one command: repeating it waits for that command while it is pending and returns its outcome afterwards, without a second restart, so omit it for a new recompile. status returns enabled, codeGeneration, fullRecompileRequired, unsupported, lastPatch and onUnsupportedEdit, also while no session runs.",
                false,
                object(&[
                    ("action", string("status,recompile"), true),
                    (
                        "requestId",
                        string(
                            "recompile: identity of the command, default a new one; a repeated id returns the outcome of its command; ASCII letters, digits or hyphens, at most 64",
                        ),
                        false,
                    ),
                ]),
            ),
            "HotReloadInput",
            &[],
            Kind::HotReload,
        ),
    ]
}
fn project_action_tools() -> Vec<StaticTool> {
    use schema::{ANY, array, boolean, map, object, optional_boolean, string};
    vec![
        static_tool(
            write_tool(
                "project_prepare",
                "Prepare an explicit saved scene using our package resolver, authored Player assembly graph, source/native-resource scene validation and the SDK compiler. Authored metadata identities are checked first. Without hostPlayer the newest valid host an earlier preparation built for this project is found and reused; only a project without one runs the Editor bootstrap, in owned staging copies that never change Packages/manifest.json. Poll identical arguments/requestId while pending; the pending reply carries progress with the current stage and its log path. Preparation completion is separate from Player readiness; activate can select the result for this controller.",
                true,
                object(&[
                    (
                        "scene",
                        string("authored Assets/...unity scene to prepare"),
                        true,
                    ),
                    ("requestId", string(""), true),
                    (
                        "hostPlayer",
                        string("optional explicitly compatible native Mono host Player"),
                        false,
                    ),
                    (
                        "nativeCatalog",
                        string("optional verified native import catalog for this project/profile"),
                        false,
                    ),
                    (
                        "engineProfile",
                        string(
                            "optional cached pinned native engine/host profile; target source graph is built from source",
                        ),
                        false,
                    ),
                    (
                        "activate",
                        optional_boolean(
                            "whether to activate the resulting baseline in this controller",
                        ),
                        false,
                    ),
                    // true builds a new host instead of reusing an earlier one.
                    ("rebuildHost", boolean(), false),
                ]),
            ),
            "ProjectPrepareInput",
            &[],
            Kind::ProjectPrepare,
        ),
        // `development` is optional on input although the DTO always forwards it.
        static_tool(
            write_tool(
                "project_build",
                "Build explicit authored scenes through the public Unity Editor build backend into a new owned output directory. development defaults to false for a release build. Poll identical arguments/requestId while status is pending. This invokes Unity BuildPipeline in the target project's own Editor and reports its actual result. That Editor needs com.unitymicroeditor.runtime declared in the project's Packages/manifest.json; the tool never adds it and fails with runtime_package_missing otherwise.",
                true,
                object(&[
                    (
                        "scenes",
                        array(
                            &string(""),
                            "explicit authored scenes for the public Unity build backend",
                        ),
                        true,
                    ),
                    ("development", boolean(), false),
                    ("requestId", string(""), true),
                ]),
            ),
            "ProjectBuildInput",
            &[],
            Kind::ProjectBuild,
        ),
        static_tool(
            write_tool(
                "prefab_edit",
                "Create, instantiate, edit, apply/revert overrides or unpack authored prefabs through our source resolver and multi-file CAS journal. add_component uses a version-bound cached native template. Backend raw_source; no Editor is started. expectedRevision/operationId are required; poll identical arguments/operationId while status is pending. Target by fileId or targetPath. Unsupported unresolved/model or structural changes fail explicitly; publication does not imply runtime import/compile success.",
                true,
                object(&[
                    (
                        "action",
                        string(
                            "instantiate,create_prefab,create_variant,apply,revert,unpack,set_override,add_component",
                        ),
                        true,
                    ),
                    (
                        "path",
                        string("authored Assets scene or prefab to edit"),
                        true,
                    ),
                    ("expectedRevision", string(""), true),
                    ("operationId", string(""), true),
                    ("requestId", string(""), false),
                    ("fileId", string(""), false),
                    ("targetPath", string(""), false),
                    ("prefabPath", string(""), false),
                    ("outputPath", string(""), false),
                    ("parentFileId", string(""), false),
                    ("parentPath", string(""), false),
                    ("name", string(""), false),
                    ("completely", boolean(), false),
                    ("componentType", string(""), false),
                    ("property", string(""), false),
                    ("value", ANY.into(), false),
                    ("fields", map(ANY, ""), false),
                ]),
            ),
            "PrefabEditInput",
            &["value"],
            Kind::PrefabEdit,
        ),
        static_tool(
            read_tool(
                "component_schema",
                "Read a supported native component's serialized defaults and required companions from the exact version/revision/native-context cache. Never starts Editor. A missing cache requires one explicit cache-component-defaults capture. Does not change authored target Assets.",
                object(&[
                    ("componentType", string(""), true),
                    ("requestId", string(""), true),
                ]),
            ),
            "ComponentSchemaInput",
            &[],
            Kind::ComponentSchema,
        ),
        static_tool(
            write_tool(
                "component_add",
                "Attach a supported native component using version-bound cached defaults and required companions in one source transaction. Never starts Editor; preserves existing GUIDs and Transform identity for RectTransform replacement. Poll identical arguments/operationId while pending. Prefab instance edits require prefab_edit. Does not imply runtime import/compile success.",
                true,
                object(&[
                    ("path", string(""), true),
                    ("fileId", string(""), true),
                    ("componentType", string(""), true),
                    ("expectedRevision", string(""), true),
                    ("operationId", string(""), true),
                    ("requestId", string(""), false),
                    (
                        "fields",
                        map(
                            ANY,
                            "explicit serialized field overrides applied to publicly captured native defaults",
                        ),
                        false,
                    ),
                ]),
            ),
            "ComponentAddInput",
            &[],
            Kind::ComponentAdd,
        ),
    ]
}

impl Server {
    pub fn new(options: &Options) -> Result<Server, AuthoringError> {
        let store = Store::new(Path::new(&options.project))?;
        let mut tools = source_tools(options.runtime.is_some());
        if options.runtime.is_some() {
            tools.extend(runtime_tools());
        }
        if options.project_actions.is_some() {
            tools.extend(project_action_tools());
        }
        Ok(Server(Arc::new(Inner {
            store,
            runtime: options.runtime.clone(),
            project_actions: options.project_actions.clone(),
            backends: options.backends.clone(),
            tools,
            dynamic: Mutex::default(),
            catalog: Mutex::default(),
            listeners: Mutex::default(),
        })))
    }
    /// The registered tools ordered by name, as `tools/list` returns them.
    pub fn tools(&self) -> Vec<ToolDefinition> {
        let mut tools: Vec<_> = self
            .0
            .tools
            .iter()
            .map(|tool| tool.definition.clone())
            .collect();
        tools.extend(
            self.0
                .dynamic
                .lock()
                .unwrap()
                .values()
                .map(|tool| tool.definition.clone()),
        );
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }
    pub fn tool(&self, name: &str) -> Option<ToolDefinition> {
        if let Some(tool) = self.0.dynamic.lock().unwrap().get(name) {
            return Some(tool.definition.clone());
        }
        self.0
            .tools
            .iter()
            .find(|tool| tool.definition.name == name)
            .map(|tool| tool.definition.clone())
    }
    /// The resources ordered by URI, as `resources/list` returns them.
    pub fn resources(&self) -> Vec<ResourceDefinition> {
        vec![
            ResourceDefinition {
                uri: CAPABILITIES_URI,
                name: "Authoring capabilities",
                mime_type: "application/json",
            },
            ResourceDefinition {
                uri: PROJECT_URI,
                name: "Bound Unity project",
                mime_type: "application/json",
            },
        ]
    }
    /// Calls `listener` whenever a `project.<name>` tool is added, replaced or
    /// removed (the transport then sends `notifications/tools/list_changed`).
    pub fn on_tools_changed(&self, listener: impl Fn() + Send + Sync + 'static) {
        self.0.listeners.lock().unwrap().push(Arc::new(listener));
    }
    fn tools_changed(&self) {
        let listeners = self.0.listeners.lock().unwrap().clone();
        for listener in listeners {
            listener();
        }
    }

    // The store bound to `ctx`, so that every store call follows the request context.
    fn with_store<T>(&self, ctx: &Context, action: impl FnOnce(&Store) -> T) -> T {
        let flag = Arc::new(AtomicBool::new(false));
        let done = Flag::new();
        let watcher = {
            let (ctx, flag, done) = (ctx.clone(), flag.clone(), done.clone());
            thread::spawn(move || {
                let sources = [cancel::Source::Flag(&done), cancel::Source::Context(&ctx)];
                if cancel::select(&sources, None) == Some(1) {
                    flag.store(true, Ordering::Relaxed);
                }
            })
        };
        let result = action(&self.0.store.with_cancellation(flag));
        done.set();
        let _ = watcher.join();
        result
    }
    /// The `instructions` of `initialize`.
    pub fn instructions(&self) -> String {
        match self.0.runtime {
            Some(_) => format!("{INSTRUCTIONS}{SESSION_INSTRUCTIONS}"),
            None => INSTRUCTIONS.to_string(),
        }
    }
    fn capabilities(&self) -> Result<Vec<u8>, HandlerError> {
        // Numeric keys are ordered as strings.
        let types: BTreeMap<String, &str> = native_component_types()
            .into_iter()
            .map(|(id, name)| (id.to_string(), name))
            .collect();
        let part = |value: Result<Vec<u8>, String>| {
            value
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .map_err(HandlerError::Failed)
        };
        // Only a server that controls a Player session has the `hot_reload` tool.
        let hot_reload = match self.0.runtime {
            Some(_) => format!(
                "\"hotReload\":{},",
                part(jsondoc::marshal(&json!({
                    "appliedLive": "A saved C# edit is compiled and applied to the running Player when possible; the Player keeps its process and its game state. session_control(wait_update) reports the outcome and the session state counts hotReload.codeGeneration.",
                    "fullRecompile": "An edit that cannot be applied live is refused, never approximated: the Player keeps the previous code, session_control(wait_update) and session_control(wait) fail with the error code full_recompile_required, and the session state has hotReload.fullRecompileRequired true with the members and reasons. Call hot_reload with action recompile; it restarts the Player on the new code and resets game state.",
                    "errorCode": "full_recompile_required",
                    "recompile": {"tool": "hot_reload", "arguments": {"action": "recompile"}},
                    "launchArguments": {
                        "hotReload": "boolean, default true; false restarts the Player on every C# edit",
                        "onUnsupportedEdit": "report (default) or restart: the Player restarts by itself for an edit that cannot be applied live",
                    },
                })))?
            ),
            None => String::new(),
        };
        Ok(format!(
            "{{\"authoring\":{},\"backends\":{},{hot_reload}\"nativeComponentTypes\":{},\"projectActionsConfigured\":{},\"runtimeConfigured\":{}}}",
            part(jsondoc::marshal(&self.0.store.capabilities()))?,
            part(jsondoc::marshal(&self.0.backends))?,
            part(jsondoc::marshal(&types))?,
            self.0.project_actions.is_some(),
            self.0.runtime.is_some(),
        )
        .into_bytes())
    }

    /// `tools/call`. `arguments` is the raw `params.arguments` JSON, absent
    /// when the request has none.
    pub fn call_tool(
        &self,
        ctx: &Context,
        name: &str,
        arguments: Option<&[u8]>,
    ) -> Result<ToolResult, CallError> {
        let dynamic = self.0.dynamic.lock().unwrap().get(name).cloned();
        if let Some(tool) = dynamic {
            return self
                .call_dynamic(ctx, &tool, arguments.unwrap_or_default())
                .map_err(CallError::Failed);
        }
        let Some(tool) = self
            .0
            .tools
            .iter()
            .find(|tool| tool.definition.name == name)
        else {
            return Err(CallError::UnknownTool(name.to_string()));
        };
        if tool.kind == Kind::ProjectTools {
            return self
                .call_project_tools(ctx, tool, arguments.unwrap_or_default())
                .map_err(CallError::Failed);
        }
        let raw = match arguments {
            Some(raw) if !raw.is_empty() => raw,
            _ => b"{}",
        };
        let (data, error) = match self.call_static(ctx, tool, raw) {
            Ok(outcome) => outcome,
            Err(error) => (Data::Nil, Some(error)),
        };
        reply(data, error).map_err(CallError::Failed)
    }

    // Validate an f64 copy against the schema, then decode the
    // original bytes into the typed input.
    fn decode<T: DeserializeOwned>(tool: &StaticTool, raw: &[u8]) -> Result<T, HandlerError> {
        let json = jsondoc::parse(raw).map_err(invalid_request)?;
        let instance = jsondoc::to_value(&json, true).map_err(invalid_request)?;
        tool.resolved.validate(&instance).map_err(invalid_request)?;
        check_integers(&tool.schema, &json, tool.target_type, "").map_err(invalid_request)?;
        serde_json::from_slice(&normalize(&json, raw, tool.raw_fields))
            .map_err(|error| invalid_request(error.to_string()))
    }
    fn call_static(
        &self,
        ctx: &Context,
        tool: &StaticTool,
        raw: &[u8],
    ) -> Result<(Data, Option<HandlerError>), HandlerError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Empty {}
        let done = |data: Data| Ok((data, None));
        match tool.kind {
            Kind::Inspect => {
                let _: Empty = Self::decode(tool, raw)?;
                done(Data::json(&self.with_store(ctx, |store| store.inspect())?)?)
            }
            Kind::Capabilities => {
                let _: Empty = Self::decode(tool, raw)?;
                done(Data::Json(self.capabilities()?))
            }
            Kind::Inventory => {
                let input: InventoryRequest = Self::decode(tool, raw)?;
                done(Data::json(
                    &self.with_store(ctx, |store| store.inventory(&input))?,
                )?)
            }
            Kind::AssetRead | Kind::ScriptRead => {
                let input: ReadInput = Self::decode(tool, raw)?;
                let source = self.with_store(ctx, |store| {
                    if tool.kind == Kind::ScriptRead {
                        store.read_script(&input.path)
                    } else {
                        store.read(&input.path)
                    }
                })?;
                done(Data::json(&read_page(source, &input)?)?)
            }
            Kind::Hierarchy => {
                let input: HierarchyRequest = Self::decode(tool, raw)?;
                done(Data::json(
                    &self.with_store(ctx, |store| store.hierarchy(&input))?,
                )?)
            }
            Kind::SceneEdit => {
                let input: EditRequest = Self::decode(tool, raw)?;
                let result = self.with_store(ctx, |store| store.edit(&input))?;
                done(Data::Edit(Box::new(result)))
            }
            Kind::AssetCreate => {
                let input: AssetCreateRequest = Self::decode(tool, raw)?;
                let result = self.with_store(ctx, |store| store.create_asset(&input))?;
                done(Data::Mutation(Box::new(result)))
            }
            Kind::ScriptCreate | Kind::ScriptEdit => {
                let input: ScriptRequest = Self::decode(tool, raw)?;
                let result = self.with_store(ctx, |store| {
                    if tool.kind == Kind::ScriptCreate {
                        store.create_script(&input)
                    } else {
                        store.edit_script(&input)
                    }
                })?;
                done(Data::Mutation(Box::new(result)))
            }
            Kind::SourceUndo => {
                let input: UndoInput = Self::decode(tool, raw)?;
                let revisions = input.expected_revisions.unwrap_or_default();
                if input.path.is_empty() || !revisions.is_empty() {
                    let request = PublicationUndoRequest {
                        operation_id: input.operation_id,
                        original_operation_id: input.original_operation_id,
                        expected_revisions: revisions,
                    };
                    let result = self.with_store(ctx, |store| store.undo_publication(&request))?;
                    return done(Data::Publication(Box::new(result)));
                }
                let request = UndoRequest {
                    path: input.path,
                    expected_revision: input.expected_revision,
                    operation_id: input.operation_id,
                    original_operation_id: input.original_operation_id,
                };
                let result = self.with_store(ctx, |store| store.undo(&request))?;
                done(Data::Mutation(Box::new(result)))
            }
            Kind::SessionControl => {
                let input: SessionInput = Self::decode(tool, raw)?;
                match input.action.as_str() {
                    "start" | "stop" | "status" | "reset" | "wait" | "wait_update" => {
                        Ok(self.forward_session(ctx, &input))
                    }
                    _ => Err(typed("unsupported_operation", "unknown session action")),
                }
            }
            Kind::RuntimeQuery => {
                let input: RuntimeInput = Self::decode(tool, raw)?;
                match input.operation.as_str() {
                    "tools" => Ok(self.refresh(
                        ctx,
                        &ProjectToolsInput {
                            request_id: input.request_id,
                            expected_pid: input.expected_pid,
                            expected_revision: input.expected_revision,
                        },
                    )),
                    "status" | "hierarchy" | "inspect" | "logs" | "raycast" => {
                        Ok(self.forward_runtime(ctx, "runtime_query", &input.marshal()))
                    }
                    _ => Err(typed(
                        "unsupported_operation",
                        "runtime_query permits only status, hierarchy, inspect, logs, raycast and tools",
                    )),
                }
            }
            Kind::RuntimeCommand => {
                let input: RuntimeInput = Self::decode(tool, raw)?;
                match input.operation.as_str() {
                    "screenshot" | "pause" | "resume" | "wait_frames" | "custom" | "invoke"
                    | "simulation_pause" | "simulation_resume" | "simulation_step" | "input" => {
                        Ok(self.forward_runtime(ctx, "runtime_command", &input.marshal()))
                    }
                    _ => Err(typed("unsupported_operation", "unknown runtime command")),
                }
            }
            Kind::RemoteSession => {
                let input: RemoteInput = Self::decode(tool, raw)?;
                match input.action.as_str() {
                    "start" | "stop" | "status" => {
                        Ok(self.forward_runtime(ctx, "remote_session", &input.marshal()))
                    }
                    _ => Err(typed(
                        "unsupported_operation",
                        "unknown remote session action",
                    )),
                }
            }
            Kind::HotReload => {
                let input: HotReloadInput = Self::decode(tool, raw)?;
                match input.action.as_str() {
                    "status" | "recompile" => Ok(self.forward_hot_reload(ctx, &input)),
                    _ => Err(typed("unsupported_operation", "unknown hot reload action")),
                }
            }
            Kind::ProjectPrepare => {
                let input: ProjectPrepareInput = Self::decode(tool, raw)?;
                Ok(self.forward_action(ctx, "project_prepare", &input.marshal()))
            }
            Kind::ProjectBuild => {
                let input: ProjectBuildInput = Self::decode(tool, raw)?;
                Ok(self.forward_action(ctx, "project_build", &input.marshal()))
            }
            Kind::PrefabEdit => {
                let input: PrefabEditInput = Self::decode(tool, raw)?;
                Ok(self.forward_action(ctx, "prefab_edit", &input.marshal()))
            }
            Kind::ComponentSchema => {
                let input: ComponentSchemaInput = Self::decode(tool, raw)?;
                Ok(self.forward_action(ctx, "component_schema", &input.marshal()))
            }
            Kind::ComponentAdd => {
                let input: ComponentAddInput = Self::decode(tool, raw)?;
                Ok(self.forward_action(ctx, "component_add", &input.marshal()))
            }
            Kind::ProjectTools => unreachable!("project_tools takes the SDK path"),
        }
    }

    fn runtime(&self) -> &dyn RuntimeHandler {
        self.0
            .runtime
            .as_deref()
            .expect("runtime tools are registered only with a runtime backend")
    }
    fn forward_runtime(
        &self,
        ctx: &Context,
        name: &str,
        input: &[u8],
    ) -> (Data, Option<HandlerError>) {
        let (data, error) = call_backend(ctx, name, input, self.runtime(), "runtime_rejected");
        (data.into(), error)
    }
    fn forward_session(&self, ctx: &Context, input: &SessionInput) -> (Data, Option<HandlerError>) {
        let (result, error) = call_backend(
            ctx,
            "session_control",
            &input.marshal(),
            self.runtime(),
            "runtime_rejected",
        );
        if matches!(input.action.as_str(), "stop" | "reset") {
            self.clear();
        }
        if error.is_none() && matches!(input.action.as_str(), "wait" | "wait_update") {
            return (self.refresh_ready(ctx, result), None);
        }
        (result.into(), error)
    }
    fn forward_hot_reload(
        &self,
        ctx: &Context,
        input: &HotReloadInput,
    ) -> (Data, Option<HandlerError>) {
        let (result, error) = call_backend(
            ctx,
            "hot_reload",
            &input.marshal(),
            self.runtime(),
            "runtime_rejected",
        );
        // A recompile replaces the Player process, so its project tools are
        // read again as after a confirmed update.
        if error.is_none() && input.action == "recompile" {
            return (self.refresh_ready(ctx, result), None);
        }
        (result.into(), error)
    }
    fn forward_action(
        &self,
        ctx: &Context,
        name: &str,
        input: &[u8],
    ) -> (Data, Option<HandlerError>) {
        let handler = self
            .0
            .project_actions
            .as_deref()
            .expect("project action tools are registered only with a backend");
        let (data, error) = call_backend(ctx, name, input, handler, "project_action_rejected");
        (data.into(), error)
    }

    fn clear(&self) {
        let mut fingerprints = self.0.catalog.lock().unwrap();
        self.clear_locked(&mut fingerprints);
    }
    fn clear_locked(&self, fingerprints: &mut BTreeMap<String, String>) {
        let names: Vec<String> = fingerprints.keys().cloned().collect();
        self.remove_tools(&names);
        fingerprints.clear();
    }
    fn remove_tools(&self, names: &[String]) {
        let removed = {
            let mut dynamic = self.0.dynamic.lock().unwrap();
            names
                .iter()
                .filter(|name| dynamic.remove(*name).is_some())
                .count()
        };
        if removed > 0 {
            self.tools_changed();
        }
    }

    fn refresh(&self, ctx: &Context, input: &ProjectToolsInput) -> (Data, Option<HandlerError>) {
        let mut fingerprints = self.0.catalog.lock().unwrap();
        let request = runtime_input(
            "tools",
            None,
            &input.request_id,
            input.expected_pid,
            &input.expected_revision,
        );
        let (result, error) = call_backend(
            ctx,
            "runtime_query",
            &request,
            self.runtime(),
            "runtime_rejected",
        );
        if error.is_some() {
            self.clear_locked(&mut fingerprints);
            return (result.into(), error);
        }
        let mut receipt = match result {
            BackendData::Structured(receipt) => {
                self.clear_locked(&mut fingerprints);
                return (BackendData::Structured(receipt).into(), None);
            }
            BackendData::Original(value) => match object_value(value.bytes()) {
                Ok(receipt) => receipt.unwrap_or_default(),
                Err(error) => {
                    self.clear_locked(&mut fingerprints);
                    return (Data::Nil, Some(HandlerError::Failed(error)));
                }
            },
        };
        match self.publish(&mut fingerprints, input, &mut receipt) {
            Ok(()) => (BackendData::Structured(receipt).into(), None),
            Err(error) => (Data::Nil, Some(error)),
        }
    }
    fn publish(
        &self,
        fingerprints: &mut BTreeMap<String, String>,
        input: &ProjectToolsInput,
        receipt: &mut Map<String, Value>,
    ) -> Result<(), HandlerError> {
        let nested = receipt.get("result").is_some_and(Value::is_object);
        let descriptors = {
            let payload = if nested {
                receipt["result"].as_object().expect("checked")
            } else {
                &*receipt
            };
            match payload.get("tools").and_then(Value::as_array) {
                Some(descriptors) => descriptors.clone(),
                None => {
                    self.clear_locked(fingerprints);
                    return Err(typed(
                        "invalid_tool_catalog",
                        "runtime tools receipt requires a tools array",
                    ));
                }
            }
        };
        if descriptors.len() > 256 {
            self.clear_locked(fingerprints);
            return Err(typed(
                "tool_catalog_too_large",
                "runtime tool catalog exceeds the 256-tool bound",
            ));
        }
        let mut pid = input.expected_pid;
        if let Some(value) = receipt.get("pid").filter(|value| value.is_number()) {
            pid = value.as_f64().unwrap_or_default() as i64;
        }
        let mut revision = input.expected_revision.clone();
        if let Some(value) = receipt.get("revision").and_then(Value::as_str) {
            revision = value.to_string();
        }
        if pid <= 0 || revision.is_empty() {
            self.clear_locked(fingerprints);
            return Err(typed(
                "invalid_tool_catalog",
                "runtime tools receipt requires a Player PID and revision",
            ));
        }
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for descriptor in &descriptors {
            if let Some(name) = descriptor.get("name").and_then(Value::as_str)
                && descriptor.is_object()
            {
                *counts.entry(name).or_default() += 1;
            }
        }
        let mut accepted = BTreeMap::new();
        let mut errors: Vec<Value> = Vec::new();
        let mut changed = false;
        for descriptor in &descriptors {
            let Some(value) = descriptor.as_object() else {
                errors.push(json!({"code": "invalid_descriptor", "message": "tool descriptor must be an object"}));
                continue;
            };
            let name = text(value, "name");
            let mut reject = |code: &str, message: &str| {
                errors.push(json!({"name": name, "code": code, "message": message}));
            };
            if !tool_name().is_match(name) || counts.get(name) != Some(&1) {
                reject(
                    "invalid_name",
                    "MCP project tool names require unique 1..120 ASCII letters/digits/dots/underscores/hyphens",
                );
                continue;
            }
            let Some(parameter_schema) = value
                .get("inputSchema")
                .and_then(Value::as_object)
                .filter(|schema| schema.get("type").and_then(Value::as_str) == Some("object"))
            else {
                reject("invalid_schema", "DTO input schema must have object type");
                continue;
            };
            let read_only = value
                .get("readOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let binding = text(value, "binding");
            let requires_target = binding == "component"
                || value.get("requiresTarget").and_then(Value::as_bool) == Some(true);
            if !matches!(binding, "static" | "component" | "legacy") {
                reject(
                    "invalid_binding",
                    "tool binding must be static,component or legacy",
                );
                continue;
            }
            let mut properties = json!({
                "requestId": {"type": "string", "minLength": 1, "maxLength": 64, "pattern": "^[A-Za-z0-9-]+$"},
                "expectedPid": {"type": "integer", "minimum": 1},
                "expectedRevision": {"type": "string", "minLength": 1, "maxLength": 128},
            });
            // Inserted as is: `json!` would rewrite the number text of the DTO schema.
            properties["parameters"] = Value::Object(parameter_schema.clone());
            let mut required = vec!["parameters"];
            if !read_only {
                required.push("requestId");
            }
            if requires_target {
                properties["target"] = json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {"type": "string", "minLength": 1, "maxLength": 2048},
                        "fileId": {"type": "string", "minLength": 1, "maxLength": 2048},
                    },
                    "oneOf": [{"required": ["path"]}, {"required": ["fileId"]}],
                });
                required.push("target");
            }
            let mut input_schema = json!({
                "type": "object",
                "additionalProperties": false,
                "required": required,
            });
            input_schema["properties"] = properties;
            let encoded = jsondoc::encode(&input_schema);
            if encoded.len() > 1 << 20 {
                reject("schema_too_large", "DTO wrapper schema exceeds one MiB");
                continue;
            }
            let resolved = match Resolved::parse(&input_schema) {
                Ok(resolved) => resolved,
                Err(error) => {
                    reject("invalid_schema", &error);
                    continue;
                }
            };
            let mcp_name = format!("project.{name}");
            // Compact JSON of `{"Descriptor", "PID", "Revision"}`, in that order.
            let mut fingerprint_source = b"{\"Descriptor\":".to_vec();
            fingerprint_source.extend(jsondoc::encode(descriptor));
            fingerprint_source.extend_from_slice(
                format!(
                    ",\"PID\":{pid},\"Revision\":{}}}",
                    jsondoc::quote(&revision)
                )
                .as_bytes(),
            );
            let fingerprint = sha256(&fingerprint_source);
            accepted.insert(mcp_name.clone(), fingerprint.clone());
            if fingerprints.get(&mcp_name) == Some(&fingerprint) {
                continue;
            }
            let description = format!(
                "{}{PROJECT_TOOL_SUFFIX}",
                truncate_description(text(value, "description"))
            );
            let tool = DynamicTool {
                definition: ToolDefinition {
                    name: mcp_name.clone(),
                    description,
                    input_schema: String::from_utf8(encoded).expect("JSON is UTF-8"),
                    output_schema: None,
                    read_only,
                    idempotent: false,
                },
                name: name.to_string(),
                resolved,
                pid,
                revision: revision.clone(),
            };
            self.0
                .dynamic
                .lock()
                .unwrap()
                .insert(mcp_name, Arc::new(tool));
            changed = true;
        }
        if changed {
            self.tools_changed();
        }
        let removed: Vec<String> = fingerprints
            .keys()
            .filter(|name| !accepted.contains_key(*name))
            .cloned()
            .collect();
        self.remove_tools(&removed);
        let registered = accepted.len();
        *fingerprints = accepted;
        let payload = if nested {
            receipt["result"].as_object_mut().expect("checked")
        } else {
            receipt
        };
        if !errors.is_empty() {
            payload.insert("mcpCatalogErrors".into(), Value::Array(errors));
        }
        payload.insert("mcpRegisteredCount".into(), registered.into());
        Ok(())
    }
    fn refresh_ready(&self, ctx: &Context, result: BackendData) -> Data {
        let state = match &result {
            BackendData::Structured(state) => Some(state.clone()),
            BackendData::Original(value) => object_value(value.bytes()).ok().flatten(),
        };
        let Some(mut state) = state.filter(|state| text(state, "phase") == "ready") else {
            return result.into();
        };
        let pid = state
            .get("pid")
            .filter(|value| value.is_number())
            .and_then(Value::as_f64)
            .unwrap_or_default() as i64;
        let revision = text(&state, "revision").to_string();
        let refresh_context = ctx.with_timeout(Duration::from_secs(5));
        let (data, error) = self.refresh(
            &refresh_context,
            &ProjectToolsInput {
                request_id: String::new(),
                expected_pid: pid,
                expected_revision: revision,
            },
        );
        refresh_context.cancel();
        match error {
            Some(error) => {
                self.clear();
                state.insert("projectToolsRefreshError".into(), error.message().into());
            }
            None => {
                let status = match &data {
                    Data::Map(_, receipt) => text(receipt, "status").to_string(),
                    _ => String::new(),
                };
                if status.is_empty() {
                    state.insert("projectToolsRefreshed".into(), true.into());
                } else {
                    self.clear();
                    state.insert("projectToolsRefreshStatus".into(), status.into());
                }
            }
        }
        BackendData::Structured(state).into()
    }

    // The typed-tool path for `project_tools`: arguments are validated and
    // re-encoded with f64 numbers, failures are plain text results, and the
    // structured content is the f64 form of the reply.
    fn call_project_tools(
        &self,
        ctx: &Context,
        tool: &StaticTool,
        raw: &[u8],
    ) -> Result<ToolResult, String> {
        let failure = |message: String| {
            Ok(ToolResult {
                text: message,
                structured: None,
                is_error: true,
            })
        };
        let arguments = if raw.is_empty() {
            Map::new()
        } else {
            match jsondoc::parse(raw).and_then(|json| match json {
                Json::Null => Ok(Map::new()),
                Json::Object(members) => jsondoc::to_map(&members, true),
                // The decoding error text for a value that is not an object.
                _ => Err(format!(
                    "json: cannot unmarshal {} into value of type object",
                    jsondoc::quoted_text(String::from_utf8_lossy(raw).trim())
                )),
            }) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return failure(format!(
                        "validating \"arguments\": unmarshaling arguments: {error}"
                    ));
                }
            }
        };
        let arguments = Value::Object(arguments);
        if let Err(error) = tool.resolved.validate(&arguments) {
            return failure(format!("validating \"arguments\": {error}"));
        }
        let mut input = ProjectToolsInput::default();
        for (key, value) in arguments.as_object().expect("object") {
            let outcome = match (key.as_str(), value) {
                ("requestId", Value::String(text)) => {
                    input.request_id = text.clone();
                    Ok(())
                }
                ("expectedRevision", Value::String(text)) => {
                    input.expected_revision = text.clone();
                    Ok(())
                }
                ("expectedPid", Value::Number(number)) => number
                    .to_string()
                    .parse()
                    .map(|pid| input.expected_pid = pid)
                    // The decoding error text for a PID that is not an integer.
                    .map_err(|_| {
                        format!(
                            "json: cannot unmarshal \"{number}\" into field {}.{key} of type int",
                            tool.target_type
                        )
                    }),
                _ => Ok(()),
            };
            if let Err(error) = outcome {
                return failure(error);
            }
        }
        let (data, error) = self.refresh(ctx, &input);
        let mut result = reply(data, error)?;
        let structured = jsondoc::parse(result.text.as_bytes())
            .and_then(|json| jsondoc::to_value(&json, true))
            .map_err(|error| format!("validating tool output: unmarshaling output: {error}"))?;
        result.structured =
            Some(String::from_utf8(jsondoc::encode(&structured)).map_err(|e| e.to_string())?);
        Ok(result)
    }

    // The handler `refresh` registers for `project.<name>`.
    fn call_dynamic(
        &self,
        ctx: &Context,
        tool: &DynamicTool,
        raw: &[u8],
    ) -> Result<ToolResult, String> {
        let refuse = |message: String| reply(Data::Nil, Some(invalid_request(message)));
        let json = match jsondoc::parse(raw) {
            Ok(json) => json,
            Err(error) => return refuse(error),
        };
        let instance = match jsondoc::to_value(&json, true) {
            Ok(instance) => instance,
            Err(error) => return refuse(error),
        };
        if let Err(error) = tool.resolved.validate(&instance) {
            return refuse(error);
        }
        let Json::Object(members) = &json else {
            return refuse(Target::VALUE.mismatch(json.kind(), "projectToolInput"));
        };
        let at = |field: &'static str| Target {
            strukt: "projectToolInput",
            field,
        };
        let raw_member = |key: &str| {
            last(members, key).map(|member| {
                String::from_utf8(jsondoc::compact(member.raw(raw))).expect("JSON is UTF-8")
            })
        };
        let (mut request_id, mut expected_revision, mut expected_pid) =
            (String::new(), String::new(), 0i64);
        let mut decode = || -> Result<(), String> {
            if let Some(member) = last(members, "requestId") {
                jsondoc::set_string(&mut request_id, &member.value, at("requestId"))?;
            }
            if let Some(member) = last(members, "expectedPid") {
                jsondoc::set_int(&mut expected_pid, &member.value, at("expectedPid"))?;
            }
            if let Some(member) = last(members, "expectedRevision") {
                jsondoc::set_string(
                    &mut expected_revision,
                    &member.value,
                    at("expectedRevision"),
                )?;
            }
            Ok(())
        };
        if let Err(error) = decode() {
            return refuse(error);
        }
        if expected_pid == 0 {
            expected_pid = tool.pid;
        }
        if expected_revision.is_empty() {
            expected_revision = tool.revision.clone();
        }
        // An object with `name`, `parameters` and `target`: keys in sorted order.
        let mut arguments = format!(
            "{{\"name\":{},\"parameters\":{}",
            jsondoc::quote(&tool.name),
            raw_member("parameters").unwrap_or_else(|| "null".into())
        );
        if let Some(target) = raw_member("target") {
            arguments.push_str(&format!(",\"target\":{target}"));
        }
        arguments.push('}');
        let input = runtime_input(
            "invoke",
            Some(&arguments),
            &request_id,
            expected_pid,
            &expected_revision,
        );
        let (data, error) = call_backend(
            ctx,
            "runtime_command",
            &input,
            self.runtime(),
            "runtime_rejected",
        );
        reply(data.into(), error)
    }

    /// `resources/read`.
    pub fn read_resource(&self, ctx: &Context, uri: &str) -> Result<ResourceResult, CallError> {
        let failed = |error: HandlerError| CallError::Failed(error.message());
        match uri {
            PROJECT_URI => {
                let info = self
                    .with_store(ctx, |store| store.inspect())
                    .map_err(|error| failed(error.into()))?;
                let encoded = jsondoc::marshal(&info).map_err(CallError::Failed)?;
                resource_reply(uri, encoded).map_err(failed)
            }
            CAPABILITIES_URI => {
                resource_reply(uri, self.capabilities().map_err(failed)?).map_err(failed)
            }
            _ => Err(CallError::ResourceNotFound(uri.to_string())),
        }
    }
}
