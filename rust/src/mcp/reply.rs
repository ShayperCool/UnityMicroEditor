//! The tool reply envelope and its size bound (`reply`, `resource_reply`).
use super::backend::{BackendData, HandlerError, HandlerValue, text};
use crate::{
    authoring::{AuthoringError, EditResult, MutationResult, PublicationResult},
    jsondoc,
};
use serde::Serialize;
use serde_json::{Map, Value};

pub const MAX_REPLY_BYTES: usize = 8 << 20;

/// The `data` argument of `reply`, by the kinds of value it distinguishes.
pub(crate) enum Data {
    Nil,
    /// The JSON of any other value.
    Json(Vec<u8>),
    /// An object: its encoded bytes and the decoded members, whose numbers
    /// keep the text the encoding wrote.
    Map(Vec<u8>, Map<String, Value>),
    Edit(Box<EditResult>),
    Mutation(Box<MutationResult>),
    Publication(Box<PublicationResult>),
}
impl Data {
    pub(crate) fn json<T: Serialize>(value: &T) -> Result<Data, HandlerError> {
        jsondoc::marshal(value)
            .map(Data::Json)
            .map_err(HandlerError::Failed)
    }
    fn encode(&self) -> Result<Option<Vec<u8>>, String> {
        Ok(Some(match self {
            Data::Nil => return Ok(None),
            Data::Json(bytes) | Data::Map(bytes, _) => bytes.clone(),
            Data::Edit(result) => jsondoc::marshal(result)?,
            Data::Mutation(result) => jsondoc::marshal(result)?,
            Data::Publication(result) => jsondoc::marshal(result)?,
        }))
    }
}
impl From<HandlerValue> for Data {
    fn from(value: HandlerValue) -> Self {
        match value {
            HandlerValue::Nil => Data::Nil,
            HandlerValue::Json(bytes) => Data::Json(bytes),
            HandlerValue::Map(bytes) => match serde_json::from_slice(&bytes) {
                Ok(Value::Object(map)) => Data::Map(bytes, map),
                _ => Data::Json(bytes),
            },
        }
    }
}
impl From<BackendData> for Data {
    fn from(data: BackendData) -> Self {
        match data {
            BackendData::Original(value) => value.into(),
            BackendData::Structured(map) => Data::Map(
                jsondoc::marshal(&map).expect("JSON values always serialize"),
                map,
            ),
        }
    }
}

/// A tool result built by `reply`: one text block with the encoded
/// envelope, the same JSON as structured content, and the error flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub text: String,
    /// `None` only for results the SDK builds itself (`project_tools` input errors).
    pub structured: Option<String>,
    pub is_error: bool,
}

fn error_json(error: &AuthoringError) -> Result<Vec<u8>, String> {
    let mut encoded = format!(
        "{{\"code\":{},\"message\":{}",
        jsondoc::quote(&error.code),
        jsondoc::quote(&error.message)
    )
    .into_bytes();
    // Absent, `null` and empty details are all omitted.
    if let Some(details) = &error.details
        && details.as_object().is_none_or(|map| !map.is_empty())
        && !details.is_null()
    {
        encoded.extend_from_slice(b",\"details\":");
        encoded.extend(jsondoc::marshal(details)?);
    }
    encoded.push(b'}');
    Ok(encoded)
}

pub(crate) fn reply(data: Data, error: Option<HandlerError>) -> Result<ToolResult, String> {
    let mut success = error.is_none();
    let mut status = "";
    let mut error = error;
    if error.is_none()
        && let Data::Map(_, structured) = &data
    {
        match text(structured, "status") {
            "result_pending" => {
                success = false;
                status = "pending";
            }
            "result_unknown" | "outcome_unknown" => {
                error = Some(HandlerError::Typed(AuthoringError {
                    code: "outcome_unknown".into(),
                    message:
                        "command outcome is unconfirmed; inspect the session and retry the same requestId"
                            .into(),
                    details: Some(Value::Object(structured.clone())),
                }));
                success = false;
                status = "unknown";
            }
            _ => {}
        }
    }
    let is_error = error.is_some();
    let failure = error.map(|error| match error {
        HandlerError::Typed(typed) => typed,
        HandlerError::Cancelled(message) => AuthoringError {
            code: "cancelled".into(),
            message,
            details: None,
        },
        HandlerError::Failed(message) => AuthoringError {
            code: "operation_failed".into(),
            message,
            details: None,
        },
    });
    let confirmed = if is_error { None } else { Some(&data) };
    let mut encoded = format!("{{\"success\":{success}").into_bytes();
    if !status.is_empty() {
        encoded.extend_from_slice(format!(",\"status\":{}", jsondoc::quote(status)).as_bytes());
    }
    if let Some(data) = confirmed
        && let Some(bytes) = data.encode()?
    {
        encoded.extend_from_slice(b",\"data\":");
        encoded.extend(bytes);
    }
    if let Some(failure) = &failure {
        encoded.extend_from_slice(b",\"error\":");
        encoded.extend(error_json(failure)?);
    }
    encoded.push(b'}');
    let text = String::from_utf8(encoded).map_err(|error| error.to_string())?;
    // Byte length of the encoded tool result (content, structured content, error flag).
    let wire = r#"{"content":[{"type":"text","text":"#.len()
        + jsondoc::quote(&text).len()
        + r#"}],"structuredContent":"#.len()
        + text.len()
        + if is_error {
            r#","isError":true"#.len()
        } else {
            0
        }
        + 1;
    if wire > MAX_REPLY_BYTES {
        // A size failure after a committed source write must retain the receipt
        // identity; clients can inspect/undo the confirmed revision safely.
        let mut details = Map::new();
        let mut put = |key: &str, value: Value| {
            details.insert(key.to_string(), value);
        };
        let mut known = true;
        match &data {
            Data::Edit(value) => {
                put("operationId", value.mutation.operation_id.clone().into());
                put("path", value.mutation.path.clone().into());
                put("revision", value.mutation.revision.clone().into());
                put("preview", value.preview.into());
            }
            Data::Mutation(value) => {
                put("operationId", value.operation_id.clone().into());
                put("path", value.path.clone().into());
                put("revision", value.revision.clone().into());
            }
            Data::Publication(value) => {
                put("operationId", value.operation_id.clone().into());
                put(
                    "assets",
                    serde_json::from_slice(&jsondoc::marshal(&value.assets)?)
                        .map_err(|error| error.to_string())?,
                );
                put("replayed", value.replayed.into());
            }
            Data::Map(_, value) => {
                for key in [
                    "requestId",
                    "operationId",
                    "backend",
                    "pid",
                    "revision",
                    "status",
                ] {
                    if let Some(item) = value.get(key)
                        && (item.is_string() || item.is_number())
                    {
                        put(key, item.clone());
                    }
                }
            }
            Data::Nil | Data::Json(_) => known = false,
        }
        return reply(
            Data::Nil,
            Some(HandlerError::Typed(AuthoringError {
                code: "response_too_large".into(),
                message: "result exceeds the 8 MiB MCP response bound; request smaller source/hierarchy pages, filter objects or set includeFields false".into(),
                details: known.then_some(Value::Object(details)),
            })),
        );
    }
    Ok(ToolResult {
        structured: Some(text.clone()),
        text,
        is_error,
    })
}

/// A resource read result with one JSON text content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceResult {
    pub uri: String,
    pub mime_type: String,
    pub text: String,
}

pub(crate) fn resource_reply(uri: &str, encoded: Vec<u8>) -> Result<ResourceResult, HandlerError> {
    let text =
        String::from_utf8(encoded).map_err(|error| HandlerError::failed(error.to_string()))?;
    // Byte length of the encoded resource result.
    let wire = r#"{"ttlMs":0,"cacheScope":"","contents":[{"uri":"#.len()
        + jsondoc::quote(uri).len()
        + r#","mimeType":"application/json","text":"#.len()
        + jsondoc::quote(&text).len()
        + "}]}".len();
    if wire > MAX_REPLY_BYTES {
        return Err(HandlerError::Typed(AuthoringError {
            code: "response_too_large".into(),
            message: "resource exceeds the 8 MiB MCP response bound; use project_inventory for paged inventory or asset_read/scene_hierarchy for known paths".into(),
            details: None,
        }));
    }
    Ok(ResourceResult {
        uri: uri.into(),
        mime_type: "application/json".into(),
        text,
    })
}
